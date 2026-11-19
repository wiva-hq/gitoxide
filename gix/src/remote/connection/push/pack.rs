use std::sync::atomic::{AtomicBool, Ordering};

use gix_error::{OptionExt, ResultExt, bail, cancelled, message};
use gix_hash::ObjectId;
use gix_pack::data::output;
use gix_utils::progress::{Count, DynNestedProgress, Progress};

use crate::Result;

/// Write a pack to `out` with all objects reachable from `tips` that aren't reachable from `remote_tips`,
/// which are the values of the references the remote advertised, and of which only those we have are considered.
///
/// The pack may contain some objects the remote already has, and it's never thin.
/// All objects are stored without deltas, compressed according to `pack.compression`.
pub(super) fn write(
    repo: &crate::Repository,
    tips: &[ObjectId],
    remote_tips: &[ObjectId],
    out: &mut dyn std::io::Write,
    progress: &mut dyn DynNestedProgress,
    should_interrupt: &AtomicBool,
) -> Result<()> {
    let mut repo = repo.clone();
    // Replacements are a local view, the remote must receive the objects as they are.
    repo.objects.ignore_replacements = true;

    let mut commit_tips = Vec::new();
    let mut tree_tips = Vec::new();
    let mut objects_as_is = Vec::new();
    for tip in tips {
        let mut id = *tip;
        loop {
            let object = repo.find_object(id)?;
            match object.kind {
                gix_object::Kind::Commit => {
                    commit_tips.push(id);
                    break;
                }
                gix_object::Kind::Tree => {
                    tree_tips.push(id);
                    break;
                }
                gix_object::Kind::Blob => {
                    objects_as_is.push(id);
                    break;
                }
                gix_object::Kind::Tag => {
                    objects_as_is.push(id);
                    id = object.into_tag().target_id()?.detach();
                }
            }
        }
    }
    let mut remote_commits = Vec::new();
    for id in remote_tips {
        if let Some(object) = repo.try_find_object(*id)? {
            let object = object.peel_tags_to_end()?;
            if object.kind == gix_object::Kind::Commit {
                remote_commits.push(object.id);
            }
        }
    }

    let mut counting = progress.add_child("counting objects".into());
    counting.init(None, gix_utils::progress::count("objects"));
    let new_commits = repo
        .rev_walk(commit_tips)
        .with_hidden(remote_commits)
        .all()?
        .map(|info| info.map(|info| info.id))
        .collect::<std::result::Result<Vec<_>, _>>()
        .or_raise(|| message("Could not find the commits the remote doesn't have"))?;

    let db: &gix_odb::Handle = &repo.objects;
    let mut counts = Vec::new();
    for (ids, expansion) in [
        // Each commit adds the objects that changed compared to its parents, which the remote has unless they are new too.
        (
            new_commits,
            output::count::objects::ObjectExpansion::TreeAdditionsComparedToAncestor,
        ),
        (tree_tips, output::count::objects::ObjectExpansion::TreeContents),
        (objects_as_is, output::count::objects::ObjectExpansion::AsIs),
    ] {
        if ids.is_empty() {
            continue;
        }
        let (new_counts, _outcome) = output::count::objects_unthreaded(
            db,
            &mut ids.into_iter().map(Ok::<_, gix_error::Error>),
            &counting,
            should_interrupt,
            expansion,
        )?;
        counts.extend(new_counts);
    }
    let mut seen = gix_hashtable::HashSet::default();
    counts.retain(|count| seen.insert(count.id));
    drop(counting);

    let num_entries = u32::try_from(counts.len()).or_raise(|| message("Too many objects to fit into a single pack"))?;
    let compression = repo.pack_compression()?;
    let mut writing = progress.add_child("writing objects".into());
    writing.init(Some(counts.len()), gix_utils::progress::count("objects"));
    let mut buf = Vec::new();
    let entries = counts.iter().map(|count| -> Result<Vec<output::Entry>> {
        if should_interrupt.load(Ordering::Relaxed) {
            bail!(cancelled("Interrupted while writing the pack"));
        }
        let (data, _location) = gix_pack::Find::try_find(db, &count.id, &mut buf)?
            .ok_or_raise(|| message!("Object {} to be sent is missing", count.id))?;
        let entry = output::Entry::from_data(count, &data, compression)?;
        writing.inc();
        Ok(vec![entry])
    });
    let mut pack = output::bytes::FromEntriesIter::new(
        entries,
        out,
        num_entries,
        gix_pack::data::Version::V2,
        repo.object_hash(),
    );
    for written in pack.by_ref() {
        written?;
    }
    Ok(())
}
