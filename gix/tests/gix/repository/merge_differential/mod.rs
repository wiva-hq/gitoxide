//! Compare merges by `gix` with the same merges by Git, recording every difference before failing.
use std::{collections::BTreeMap, fmt::Write as _};

use gix::{
    ObjectId,
    bstr::{BString, ByteSlice},
    merge::tree::{FileFavor, TreatAsUnresolved, TreeFavor},
};

mod corpus;
mod generated;

/// An unmerged index entry as `(path, mode, id, stage)`.
type Stage = (BString, u32, ObjectId, u8);

/// One merge as printed by `git merge-tree --stdin -z --write-tree --messages`.
struct GitMerge {
    conflicted: bool,
    tree_id: ObjectId,
    stages: Vec<Stage>,
    messages: Vec<String>,
}

/// Parse all merges printed by `git merge-tree --stdin -z --write-tree --messages`.
fn parse_merges(output: &[u8]) -> gix_testtools::TestResult<Vec<GitMerge>> {
    let mut fields = output.split(|byte| *byte == 0);
    let mut merges = Vec::new();
    while let Some(status) = fields.next().filter(|field| !field.is_empty()) {
        let conflicted = match status {
            b"0" => true,
            b"1" => false,
            other => return Err(format!("unexpected merge status {:?}", other.as_bstr()).into()),
        };
        let tree_id = ObjectId::from_hex(fields.next().ok_or("Git prints the result tree")?)?;
        let mut stages = Vec::new();
        if conflicted {
            for field in fields.by_ref().take_while(|field| !field.is_empty()) {
                let tab = field
                    .find_byte(b'\t')
                    .ok_or("stage entries separate the path with a tab")?;
                let mut info = field[..tab].split(|byte| *byte == b' ');
                let mut next = || info.next().ok_or("stage entries have mode, id and stage");
                let mode = u32::from_str_radix(next()?.to_str()?, 8)?;
                let id = ObjectId::from_hex(next()?)?;
                let stage = next()?.to_str()?.parse()?;
                stages.push((field[tab + 1..].as_bstr().to_owned(), mode, id, stage));
            }
        } else if fields.next() != Some(&[][..]) {
            return Err("clean merges are followed by an empty field".into());
        }
        let mut messages = Vec::new();
        while let Some(count) = fields.next().filter(|field| !field.is_empty()) {
            let count: usize = count.to_str()?.parse()?;
            let paths: Vec<_> = fields
                .by_ref()
                .take(count)
                .map(|path| path.as_bstr().to_owned())
                .collect();
            let kind = fields.next().ok_or("messages have a kind")?.as_bstr();
            let message = fields.next().ok_or("messages have a text")?.as_bstr();
            messages.push(format!("{kind} {paths:?}: {}", message.trim_end().as_bstr()));
        }
        stages.sort();
        merges.push(GitMerge {
            conflicted,
            tree_id,
            stages,
            messages,
        });
    }
    Ok(merges)
}

/// Return the file favor a variant name selects, which is how these tests spell `-X` options and `merge=union`.
fn file_favor(variant: &str) -> Option<FileFavor> {
    match variant {
        "ours" => Some(FileFavor::Ours),
        "theirs" => Some(FileFavor::Theirs),
        "union" => Some(FileFavor::Union),
        _ => None,
    }
}

/// Open `root` with the configuration Git used for `variant`.
fn open(root: &std::path::Path, variant: &str) -> gix_testtools::TestResult<gix::Repository> {
    let overrides: &[&str] = match variant {
        "diff3" => &["merge.conflictStyle=diff3"],
        "zdiff3" => &["merge.conflictStyle=zdiff3"],
        "renormalize" => &["merge.renormalize=true"],
        "directory-renames-true" => &["merge.directoryRenames=true"],
        "directory-renames-false" => &["merge.directoryRenames=false"],
        _ => &[],
    };
    Ok(gix::open_opts(
        root,
        gix::open::Options::isolated().config_overrides(overrides.iter().copied()),
    )?
    .with_object_memory())
}

fn tree_options(repo: &gix::Repository, variant: &str) -> gix::Result<gix::merge::tree::Options> {
    Ok(repo.tree_merge_options()?.with_file_favor(file_favor(variant)))
}

fn commit_options(repo: &gix::Repository, variant: &str) -> gix::Result<gix::merge::commit::Options> {
    Ok(gix::merge::commit::Options::from(tree_options(repo, variant)?).with_allow_missing_merge_base(true))
}

/// How a merge by `gix` relates to the same merge by Git.
enum Verdict {
    Same,
    /// The merges differ as documented in `gix_merge::tree`, *Differences from Git*.
    Known,
    /// A report of everything in which the merges differ.
    Different(String),
}

/// Compare `outcome` of merging the trees `sides` with `expected`.
fn compare(
    repo: &gix::Repository,
    expected: &GitMerge,
    outcome: &mut gix::merge::tree::Outcome<'_>,
    sides: [ObjectId; 2],
) -> gix_testtools::TestResult<Verdict> {
    let tree_id = outcome.tree.write()?.detach();
    let index = outcome.index(TreatAsUnresolved::git(), None)?;
    let mut stages: Vec<Stage> = index
        .entries()
        .iter()
        .filter(|entry| entry.stage() != gix::index::entry::Stage::Unconflicted)
        .map(|entry| {
            (
                entry.path(&index).to_owned(),
                entry.mode.bits(),
                entry.id,
                entry.stage() as u8,
            )
        })
        .collect();
    stages.sort();
    let mut listed: Vec<Stage> = outcome
        .index
        .unmerged(TreatAsUnresolved::git())
        .flat_map(|unmerged| {
            unmerged.stages.into_iter().zip(1..).filter_map(move |(entry, stage)| {
                entry.map(|(mode, id)| (unmerged.path.to_owned(), u32::from(mode.value()), id, stage))
            })
        })
        .collect();
    listed.sort();
    let conflicted = outcome.has_unresolved_conflicts(TreatAsUnresolved::git());
    let unknown = outcome.conflicts.iter().any(|conflict| {
        matches!(
            conflict.resolution,
            Err(gix::merge::tree::ResolutionFailure::Unknown)
                | Ok(gix::merge::tree::Resolution::Forced(
                    gix::merge::tree::ResolutionFailure::Unknown
                ))
        )
    });
    if tree_id == expected.tree_id
        && stages == expected.stages
        && listed == stages
        && conflicted == expected.conflicted
        && !unknown
    {
        return Ok(Verdict::Same);
    }
    let actual_entries = leaves(repo, tree_id)?;
    let expected_entries = leaves(repo, expected.tree_id)?;
    if listed == stages
        && !unknown
        && (conflicted || !expected.conflicted)
        && (is_known_rename_collision(repo, expected, sides, &actual_entries, &expected_entries, &stages)?
            || is_known_nested_rename(expected, &actual_entries, &expected_entries, &stages))
    {
        return Ok(Verdict::Known);
    }

    let mut report = String::new();
    if conflicted != expected.conflicted {
        writeln!(report, "conflicted: gix {conflicted}, Git {}", expected.conflicted)?;
    }
    if unknown {
        writeln!(report, "gix reports an unknown conflict")?;
    }
    if tree_id != expected.tree_id {
        for path in actual_entries
            .keys()
            .chain(expected_entries.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            let (actual, expected) = (actual_entries.get(path), expected_entries.get(path));
            if actual != expected {
                writeln!(
                    report,
                    "tree entry {path:?}: gix {}, Git {}",
                    describe_leaf(repo, actual)?,
                    describe_leaf(repo, expected)?
                )?;
            }
        }
    }
    if listed != stages {
        writeln!(
            report,
            "listed unmerged paths\n  listed: {listed:?}\n  index: {stages:?}"
        )?;
    }
    if stages != expected.stages {
        writeln!(
            report,
            "unmerged stages\n  gix: {stages:?}\n  Git: {:?}",
            expected.stages
        )?;
    }
    writeln!(report, "Git messages: {:#?}", expected.messages)?;
    writeln!(report, "gix conflicts: {:#?}", outcome.conflicts)?;
    Ok(Verdict::Different(report))
}

/// Return `true` if the merges only differ where both sides have the same entry, which one of them renamed
/// a changed file to. `gix` merges such a rename like any other, while Git keeps *our* entry if *theirs*
/// renamed the file, and reports a clean merge with conflict markers if *ours* did and its content conflicts.
fn is_known_rename_collision(
    repo: &gix::Repository,
    expected: &GitMerge,
    sides: [ObjectId; 2],
    actual_entries: &BTreeMap<BString, (u16, ObjectId)>,
    expected_entries: &BTreeMap<BString, (u16, ObjectId)>,
    stages: &[Stage],
) -> gix_testtools::TestResult<bool> {
    let [ours, theirs] = [leaves(repo, sides[0])?, leaves(repo, sides[1])?];
    let differing_entries = actual_entries
        .keys()
        .chain(expected_entries.keys())
        .filter(|path| actual_entries.get(*path) != expected_entries.get(*path));
    let differing_stages = stages
        .iter()
        .chain(&expected.stages)
        .filter(|stage| stages.contains(stage) != expected.stages.contains(stage))
        .map(|(path, ..)| path);
    let mut differing = differing_entries.chain(differing_stages).peekable();
    Ok(differing.peek().is_some()
        && differing.all(|path| {
            let both = ours.get(path).filter(|entry| theirs.get(path) == Some(*entry));
            both.is_some()
                && (expected_entries.get(path) == both
                    || expected.messages.iter().any(|message| {
                        message.starts_with("CONFLICT (rename involved in collision)")
                            && message.contains(&format!("{path:?}"))
                    }))
        }))
}

/// Return `true` if the merges only differ in a file Git keeps at `P` while it also lists an entry below `P`
/// as unmerged, where `gix` moves the file to `P~<side>` and keeps the entry below `P` in the tree.
fn is_known_nested_rename(
    expected: &GitMerge,
    actual_entries: &BTreeMap<BString, (u16, ObjectId)>,
    expected_entries: &BTreeMap<BString, (u16, ObjectId)>,
    stages: &[Stage],
) -> bool {
    expected.stages.iter().any(|(file, ..)| {
        let mut prefix = file.clone();
        prefix.push(b'/');
        let mut moved_prefix = file.clone();
        moved_prefix.push(b'~');
        let Some(moved) = stages
            .iter()
            .map(|(path, ..)| path)
            .find(|path| path.starts_with(&moved_prefix))
        else {
            return false;
        };
        let mut relocated: Vec<Stage> = stages
            .iter()
            .map(|(path, mode, id, stage)| (if path == moved { file } else { path }.clone(), *mode, *id, *stage))
            .collect();
        relocated.sort();
        relocated == expected.stages
            && expected.stages.iter().any(|(path, ..)| path.starts_with(&prefix))
            && actual_entries.get(moved) == expected_entries.get(file)
            && actual_entries
                .keys()
                .chain(expected_entries.keys())
                .filter(|path| actual_entries.get(*path) != expected_entries.get(*path))
                .all(|path| {
                    path == file
                        || path == moved
                        || (path.starts_with(&prefix)
                            && expected_entries.get(path).is_none()
                            && expected.stages.iter().any(|(staged, mode, id, _)| {
                                staged == path && actual_entries.get(path) == Some(&(*mode as u16, *id))
                            }))
                })
    })
}

/// Check that forced resolutions leave nothing unresolved for every conflict they apply to.
fn check_forced_resolutions<'repo>(
    repo: &'repo gix::Repository,
    merge: impl Fn(gix::merge::commit::Options) -> gix::Result<gix::merge::tree::Outcome<'repo>>,
) -> gix_testtools::TestResult<Option<String>> {
    let mut report = String::new();
    for (tree_favor, file_favor) in [
        (TreeFavor::Ours, FileFavor::Ours),
        (TreeFavor::Ours, FileFavor::Theirs),
        (TreeFavor::Ancestor, FileFavor::Theirs),
    ] {
        let options = gix::merge::commit::Options::from(
            repo.tree_merge_options()?
                .with_tree_favor(Some(tree_favor))
                .with_file_favor(Some(file_favor)),
        )
        .with_allow_missing_merge_base(true);
        let outcome = merge(options)?;
        if outcome.has_unresolved_conflicts(TreatAsUnresolved::git())
            || outcome.index.has_unmerged_entries(TreatAsUnresolved::git())
        {
            writeln!(
                report,
                "{tree_favor:?} tree favor with {file_favor:?} file favor leaves conflicts: {:#?}",
                outcome.conflicts
            )?;
        }
    }
    Ok((!report.is_empty()).then_some(report))
}

/// Return all non-tree entries below `tree_id` by their full path.
fn leaves(repo: &gix::Repository, tree_id: ObjectId) -> gix_testtools::TestResult<BTreeMap<BString, (u16, ObjectId)>> {
    let mut out = BTreeMap::new();
    let mut pending = vec![(BString::default(), tree_id)];
    while let Some((prefix, tree_id)) = pending.pop() {
        let tree = repo.find_tree(tree_id)?;
        for entry in tree.decode()?.entries {
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(entry.filename);
            if entry.mode.is_tree() {
                pending.push((path, entry.oid.to_owned()));
            } else {
                out.insert(path, (entry.mode.value(), entry.oid.to_owned()));
            }
        }
    }
    Ok(out)
}

fn describe_leaf(repo: &gix::Repository, leaf: Option<&(u16, ObjectId)>) -> gix_testtools::TestResult<String> {
    let Some((mode, id)) = leaf else {
        return Ok("absent".into());
    };
    if *mode == 0o160000 {
        return Ok(format!("{mode:o} {id}"));
    }
    let data = repo.find_blob(*id)?.data.clone();
    let shown = &data[..data.len().min(600)];
    Ok(format!(
        "{mode:o} {id} {:?}{}",
        shown.as_bstr(),
        if shown.len() < data.len() { "…" } else { "" }
    ))
}
