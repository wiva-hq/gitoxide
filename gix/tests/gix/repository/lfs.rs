use std::{io::Read, path::Path};

use gix::{
    bstr::{BStr, ByteSlice},
    lfs::{WorktreeFile, plumbing::Operation},
};
use gix_testtools::TestResult;

use crate::util::{named_repo, named_subrepo_opts, repo_rw, restricted};

const FIXTURE: &str = "make_lfs_repo.sh";

fn pointer(hex_digit: char, size: u64) -> gix::lfs::plumbing::Pointer {
    let hex = hex_digit.to_string().repeat(64);
    gix::lfs::plumbing::Pointer::new(
        gix::lfs::plumbing::Oid::from_hex(hex.as_bytes()).expect("valid hex"),
        size,
    )
}

#[test]
fn pointers_are_listed_in_trees_and_commit_ranges() -> TestResult {
    let repo = named_repo(FIXTURE)?;
    let lfs = repo.lfs();
    let (a, b) = (pointer('a', 10), pointer('b', 20));

    let in_tree = lfs.pointers_in_tree(repo.head_tree_id()?)?;
    assert_eq!(
        in_tree
            .iter()
            .map(|entry| (entry.path.to_str_lossy().into_owned(), entry.pointer.clone()))
            .collect::<Vec<_>>(),
        [
            ("dir/copy.bin".to_owned(), a.clone()),
            ("dir/second.bin".to_owned(), b.clone()),
            ("first.bin".to_owned(), a.clone()),
        ],
        "every pointer path is listed in tree order, while `small.txt` isn't a pointer"
    );

    let head_id = repo.head_id()?.detach();
    let in_history = lfs.pointers_in_commits([head_id], None::<gix::ObjectId>)?;
    assert_eq!(
        in_history.iter().map(|entry| entry.pointer.clone()).collect::<Vec<_>>(),
        [a, b.clone()],
        "the same blob is listed once in a commit range"
    );

    let first_commit_id = repo
        .find_commit(head_id)?
        .parent_ids()
        .next()
        .expect("the second commit has a parent")
        .detach();
    let introduced = lfs.pointers_in_commits([head_id], [first_commit_id])?;
    assert_eq!(
        introduced.iter().map(|entry| entry.pointer.clone()).collect::<Vec<_>>(),
        [b],
        "pointers already in hidden commits are excluded, even at other paths"
    );
    Ok(())
}

#[test]
fn clean_and_smudge_round_trip_through_the_filter_pipeline() -> TestResult {
    let (repo, _tmp) = repo_rw(FIXTURE)?;
    let (mut pipeline, index) = repo.filter_pipeline(None)?;
    let content = b"large content\n".repeat(1000);

    let mut pointer_bytes = Vec::new();
    pipeline
        .convert_to_git(content.as_slice(), Path::new("big.bin"), &index)?
        .read_to_end(&mut pointer_bytes)?;
    let pointer = gix::lfs::plumbing::Pointer::from_bytes(&pointer_bytes)?;
    assert_eq!(
        pointer.oid,
        gix::lfs::plumbing::Oid::for_content(&content),
        "cleaning yields the pointer to the content"
    );
    assert!(repo.lfs().store()?.contains(&pointer), "the content is in the store");
    assert!(
        repo.lfs().store()?.root().starts_with(repo.common_dir()),
        "the store is in the common directory by default"
    );

    let mut smudged = Vec::new();
    pipeline
        .convert_to_worktree(&pointer_bytes, "big.bin".into(), Default::default())?
        .read_to_end(&mut smudged)?;
    assert_eq!(smudged, content, "smudging yields the content again");

    let mut unchanged = Vec::new();
    pipeline
        .convert_to_git(content.as_slice(), Path::new("small.txt"), &index)?
        .read_to_end(&mut unchanged)?;
    assert_eq!(
        unchanged, content,
        "files without the lfs filter attribute are left alone"
    );
    Ok(())
}

#[test]
fn missing_objects_leave_the_pointer_in_place() -> TestResult {
    let repo = named_repo(FIXTURE)?;
    let (mut pipeline, _index) = repo.filter_pipeline(None)?;
    let pointer_bytes = pointer('a', 10).to_bytes();

    let mut smudged = Vec::new();
    pipeline
        .convert_to_worktree(&pointer_bytes, "first.bin".into(), Default::default())?
        .read_to_end(&mut smudged)?;
    assert_eq!(
        smudged, pointer_bytes,
        "without the object or a remote to get it from, the pointer stays"
    );

    let lfs = repo.lfs();
    assert_eq!(
        lfs.worktree_file("first.bin".into())?,
        WorktreeFile::Pointer {
            pointer: pointer('a', 10),
            object_in_store: false,
        },
        "the worktree file can be identified as pointer to a missing object"
    );
    assert_eq!(lfs.worktree_file("small.txt".into())?, WorktreeFile::NotAPointer);
    assert_eq!(lfs.worktree_file("missing.bin".into())?, WorktreeFile::Absent);
    Ok(())
}

#[test]
fn the_builtin_driver_can_be_disabled() -> TestResult {
    let repo = named_subrepo_opts(
        FIXTURE,
        ".",
        restricted().config_overrides(["gitoxide.core.builtinLfsFilter=false"]),
    )?;
    let (mut pipeline, index) = repo.filter_pipeline(None)?;
    let out = pipeline.convert_to_git(&b"content"[..], Path::new("big.bin"), &index)?;
    assert!(
        !out.is_changed(),
        "without the built-in driver and without configured programs, nothing is filtered"
    );
    Ok(())
}

#[test]
fn endpoints_are_configured_or_derived_from_the_remote() -> TestResult {
    let endpoint = |overrides: &[&str], operation: Operation| -> gix::Result<Option<String>> {
        let repo = named_subrepo_opts(
            FIXTURE,
            ".",
            restricted().config_overrides(overrides.iter().map(ToString::to_string)),
        )?;
        Ok(repo.lfs().endpoint(None, operation)?.map(|endpoint| endpoint.url))
    };
    assert_eq!(endpoint(&[], Operation::Download)?, None, "no remote, no endpoint");

    let https = "remote.origin.url=https://example.com/org/repo";
    assert_eq!(
        endpoint(&[https], Operation::Download)?.as_deref(),
        Some("https://example.com/org/repo.git/info/lfs"),
        "derived from the remote URL"
    );
    assert_eq!(
        endpoint(
            &[https, "remote.origin.lfsurl=https://lfs.example.com/remote"],
            Operation::Upload
        )?
        .as_deref(),
        Some("https://lfs.example.com/remote"),
        "the remote can configure it"
    );
    assert_eq!(
        endpoint(
            &[
                https,
                "remote.origin.lfsurl=https://lfs.example.com/remote",
                "lfs.url=https://lfs.example.com/all"
            ],
            Operation::Download
        )?
        .as_deref(),
        Some("https://lfs.example.com/all"),
        "lfs.url takes precedence"
    );
    let push = [https, "lfs.pushurl=https://lfs.example.com/push"];
    assert_eq!(
        endpoint(&push, Operation::Upload)?.as_deref(),
        Some("https://lfs.example.com/push"),
        "uploads can go elsewhere"
    );
    assert_eq!(
        endpoint(&push, Operation::Download)?.as_deref(),
        Some("https://example.com/org/repo.git/info/lfs"),
        "but downloads don't"
    );

    let repo = named_subrepo_opts(
        FIXTURE,
        ".",
        restricted().config_overrides(["remote.origin.url=git@example.com:org/repo.git"]),
    )?;
    let ssh = repo
        .lfs()
        .endpoint(None, Operation::Download)?
        .expect("an SSH remote has an endpoint");
    assert_eq!(
        ssh.url, "https://example.com/org/repo.git/info/lfs",
        "SSH maps to HTTPS"
    );
    let ssh = ssh.ssh.expect("SSH authentication is used");
    assert_eq!(
        (ssh.user.as_deref(), ssh.host.as_str(), ssh.path.as_str()),
        (Some("git"), "example.com", "org/repo.git"),
        "git-lfs-authenticate is run on the same host for the same repository"
    );

    let repo = named_subrepo_opts(FIXTURE, "lfsconfig", restricted())?;
    let from_lfsconfig = |repo: &gix::Repository| -> gix::Result<Option<String>> {
        Ok(repo
            .lfs()
            .endpoint(None, Operation::Download)?
            .map(|endpoint| endpoint.url))
    };
    assert_eq!(
        from_lfsconfig(&repo)?.as_deref(),
        Some("https://lfs.example.com/from-lfsconfig"),
        ".lfsconfig in the worktree is used"
    );
    let repo = named_subrepo_opts(
        FIXTURE,
        "lfsconfig",
        restricted().config_overrides(["lfs.url=https://lfs.example.com/from-git"]),
    )?;
    assert_eq!(
        from_lfsconfig(&repo)?.as_deref(),
        Some("https://lfs.example.com/from-git"),
        "Git configuration takes precedence over .lfsconfig"
    );
    Ok(())
}

#[test]
fn the_store_follows_lfs_storage() -> TestResult {
    let repo = named_subrepo_opts(FIXTURE, ".", restricted().config_overrides(["lfs.storage=elsewhere"]))?;
    assert_eq!(
        repo.lfs().store()?.root(),
        repo.common_dir().join("elsewhere"),
        "relative paths are relative to the common directory"
    );
    let path: &BStr = "first.bin".into();
    assert!(
        matches!(repo.lfs().worktree_file(path)?, WorktreeFile::Pointer { .. }),
        "the store location doesn't change what's in the worktree"
    );
    Ok(())
}
