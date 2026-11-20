use std::collections::{BTreeMap, BTreeSet};

use gix::{
    ObjectId,
    merge::plumbing::blob::{
        ResourceKind,
        builtin_driver::text::{Conflict, ConflictStyle, Labels},
    },
    merge::tree::{TreatAsUnresolved, UnmergedKind},
    objs::tree::EntryKind,
};
use gix_testtools::TestResult;

use super::{Verdict, check_forced_resolutions, commit_options, compare, open, parse_merges, tree_options};

fn fixture() -> gix_testtools::Result<Option<std::path::PathBuf>> {
    // `merge-tree` accepts `-X` since Git 2.45.
    gix_testtools::scripted_fixture_read_only_with_git_version("make_merge_differential.sh", |version| {
        version >= (2, 45, 0)
    })
}

struct Case<'a> {
    name: &'a str,
    direction: &'a str,
    ours: &'a str,
    theirs: &'a str,
}

#[test]
fn hand_written_histories_match_git() -> TestResult {
    let Some(root) = fixture()? else {
        return Ok(());
    };
    let cases = std::fs::read_to_string(root.join("cases"))?;
    let mut by_variant = BTreeMap::<&str, Vec<Case<'_>>>::new();
    for line in cases.lines() {
        let [name, direction, variant, ours, theirs] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("malformed case line: {line:?}");
        };
        by_variant.entry(variant).or_default().push(Case {
            name,
            direction,
            ours,
            theirs,
        });
    }

    let mut failures = Vec::new();
    let mut known = BTreeSet::new();
    let mut merges = 0;
    let mut unrepresentable = 0;
    for (variant, cases) in &by_variant {
        let expected = parse_merges(&std::fs::read(root.join("results").join(variant))?)?;
        assert_eq!(expected.len(), cases.len(), "{variant}: Git merged every queued pair");
        let git_tree_ids: BTreeMap<_, _> = cases
            .iter()
            .zip(&expected)
            .map(|(case, expected)| ((case.name, case.direction), expected.tree_id))
            .collect();
        let repo = open(&root, variant)?;
        for (case, expected) in cases.iter().zip(&expected) {
            let context = format!("{} {} {variant}", case.name, case.direction);
            let ours_commit_id = repo.find_reference(format!("refs/heads/{}", case.ours).as_str())?.id();
            let their_commit_id = repo
                .find_reference(format!("refs/heads/{}", case.theirs).as_str())?
                .id();
            let sides = [
                repo.find_commit(ours_commit_id)?.tree_id()?.detach(),
                repo.find_commit(their_commit_id)?.tree_id()?.detach(),
            ];
            let labels = Labels {
                ancestor: None,
                current: Some(case.ours.into()),
                other: Some(case.theirs.into()),
            };
            if cfg!(windows) && case.name.starts_with("non-utf8") {
                // Windows paths are Unicode, so `gix` can't look up attributes of these paths, unlike Git.
                match repo.merge_commits(ours_commit_id, their_commit_id, labels, commit_options(&repo, variant)?) {
                    Err(err) if format!("{err:?}").contains("Could not convert to UTF8") => unrepresentable += 1,
                    Err(err) => failures.push(format!("{context}: {err:?}")),
                    Ok(_) => failures.push(format!("{context}: merged paths that aren't valid UTF-8 on Windows")),
                }
                continue;
            }
            let mut check = |api: &str, outcome: &mut gix::merge::tree::Outcome<'_>| -> TestResult<Option<String>> {
                Ok(match compare(&repo, expected, outcome, sides)? {
                    Verdict::Same => None,
                    Verdict::Known => {
                        known.insert((case.name, case.direction, *variant));
                        let mirrored = if case.direction == "A-B" { "B-A" } else { "A-B" };
                        // Where Git keeps an entry instead of merging the rename, its other direction shows the merge.
                        (case.name == "rename-add-identical"
                            && outcome.tree.write()?.detach() != git_tree_ids[&(case.name, mirrored)])
                            .then(|| format!("{context} {api}: differs from Git's {mirrored} merge"))
                    }
                    Verdict::Different(report) => Some(format!("{context} {api}\n{report}")),
                })
            };

            merges += 1;
            let mut outcome = repo
                .merge_commits(ours_commit_id, their_commit_id, labels, commit_options(&repo, variant)?)?
                .tree_merge;
            failures.extend(check("merge_commits()", &mut outcome)?);
            if *variant == "default"
                && let Some(expected_kinds) = unmerged_kinds(case.name, case.direction == "A-B")
            {
                let kinds: Vec<_> = outcome
                    .index
                    .unmerged(TreatAsUnresolved::git())
                    .map(|unmerged| (unmerged.path.to_string(), unmerged.kind()))
                    .collect();
                if kinds != expected_kinds {
                    failures.push(format!(
                        "{context} unmerged kinds\n  gix: {kinds:?}\n  expected: {expected_kinds:?}"
                    ));
                }
            }

            let base = repo.try_find_reference(format!("refs/heads/{}-base", case.name).as_str())?;
            if let Some(base) = base.filter(|_| !variant.contains("diff3")) {
                merges += 1;
                let mut outcome = repo.merge_trees(
                    repo.find_commit(base.id())?.tree_id()?,
                    repo.find_commit(ours_commit_id)?.tree_id()?,
                    repo.find_commit(their_commit_id)?.tree_id()?,
                    labels,
                    tree_options(&repo, variant)?,
                )?;
                failures.extend(check("merge_trees()", &mut outcome)?);
            }

            if *variant == "default"
                && let Some(report) = check_forced_resolutions(&repo, |options| {
                    Ok(repo
                        .merge_commits(ours_commit_id, their_commit_id, labels, options)?
                        .tree_merge)
                })?
            {
                failures.push(format!("{context} forced resolution\n{report}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {merges} merges differ from Git:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    assert_eq!(
        known,
        BTreeSet::from([
            ("rename-add-identical", "B-A", "default"),
            ("rename-add-identical", "B-A", "ours"),
            ("rename-add-identical", "B-A", "theirs"),
            ("rename-add-identical", "B-A", "union"),
            ("rename-add-identical-conflict", "A-B", "default"),
            ("rename-add-identical-conflict", "B-A", "default"),
            ("rename-rename-nested", "A-B", "default"),
            ("rename-rename-nested", "B-A", "default"),
        ]),
        "exactly the documented differences remain"
    );
    assert_eq!(
        (merges, unrepresentable),
        if cfg!(windows) { (1232, 16) } else { (1264, 0) },
        "every recorded merge is compared, except merges of paths Windows can't represent"
    );
    Ok(())
}

#[test]
fn blob_merges_match_git_merge_file() -> TestResult {
    let Some(root) = fixture()? else {
        return Ok(());
    };
    let repo = open(&root, "default")?;
    let mut platform = repo.merge_resource_cache(Default::default())?;
    let labels = Labels {
        ancestor: Some("base".into()),
        current: Some("ours".into()),
        other: Some("theirs".into()),
    };
    let mut failures = Vec::new();
    let cases = std::fs::read_to_string(root.join("blob-cases"))?;
    for line in cases.lines() {
        let [name, variant, status, base, ours, theirs] = line.split(' ').collect::<Vec<_>>()[..] else {
            panic!("malformed blob case line: {line:?}");
        };
        for (kind, id) in [
            (ResourceKind::CommonAncestorOrBase, base),
            (ResourceKind::CurrentOrOurs, ours),
            (ResourceKind::OtherOrTheirs, theirs),
        ] {
            platform.set_resource(
                Some(ObjectId::from_hex(id.as_bytes())?),
                EntryKind::Blob,
                "merge-file".into(),
                kind,
                &repo,
            )?;
        }
        let mut options = repo.blob_merge_options()?;
        let marker_size = Conflict::DEFAULT_MARKER_SIZE.try_into()?;
        options.text.conflict = match variant {
            "default" => Conflict::Keep {
                style: ConflictStyle::Merge,
                marker_size,
            },
            "diff3" => Conflict::Keep {
                style: ConflictStyle::Diff3,
                marker_size,
            },
            "zdiff3" => Conflict::Keep {
                style: ConflictStyle::ZealousDiff3,
                marker_size,
            },
            "ours" => Conflict::ResolveWithOurs,
            "theirs" => Conflict::ResolveWithTheirs,
            "union" => Conflict::ResolveWithUnion,
            unknown => panic!("unknown blob variant {unknown}"),
        };
        let merge = platform.prepare_merge(&repo, options)?;
        let mut buf = Vec::new();
        let (pick, resolution) = merge.merge(&mut buf, labels, &repo.command_context()?)?;
        let actual = merge
            .data_by_pick(pick, &buf)
            .as_slice()
            .ok_or("blobs are small enough to be in memory")?;
        let expected = std::fs::read(root.join("results").join(format!("blob-{name}.{variant}")))?;
        let git_conflicted = status != "0";
        let gix_conflicted = resolution == gix::merge::plumbing::blob::Resolution::Conflict;
        if actual != expected.as_slice() || git_conflicted != gix_conflicted {
            failures.push(format!(
                "{name} {variant}: conflicted: gix {gix_conflicted}, Git {git_conflicted}\n  gix: {:?}\n  Git: {:?}",
                gix::bstr::BStr::new(actual),
                gix::bstr::BStr::new(&expected)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} blob merges differ from `git merge-file`:\n{}",
        failures.len(),
        cases.lines().count(),
        failures.join("\n")
    );
    Ok(())
}

/// The kinds of unmerged paths Git leaves in the index for some of the cases, with `ours` as the case's `A` side.
fn unmerged_kinds(name: &str, ours_is_a: bool) -> Option<Vec<(String, UnmergedKind)>> {
    use UnmergedKind::*;
    let (deleted_by_other, added_by_ours, added_by_other) = if ours_is_a {
        (DeletedByThem, AddedByUs, AddedByThem)
    } else {
        (DeletedByUs, AddedByThem, AddedByUs)
    };
    let kinds = match name {
        "content-conflict" => vec![("f", BothModified)],
        "add-add-different" => vec![("new", BothAdded)],
        "modify-delete" => vec![("f", deleted_by_other)],
        "rename-delete" => vec![("g", deleted_by_other)],
        "rename-rename-divergent" => vec![("f", BothDeleted), ("g", added_by_ours), ("h", added_by_other)],
        _ => return None,
    };
    Some(kinds.into_iter().map(|(path, kind)| (path.to_owned(), kind)).collect())
}
