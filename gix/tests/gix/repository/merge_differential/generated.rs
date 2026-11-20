//! Small random histories merged by Git and `gix` with every option both understand.
//!
//! Each history comes from `GIX_MERGE_SEED` and its index alone, so `GIX_MERGE_CASE=<index>` regenerates one
//! of them, and `GIX_MERGE_CASES` sets how many are generated. Failures print the values that reproduce them.
use std::{collections::BTreeMap, fmt::Write as _, io::Write as _, path::Path};

use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice},
    merge::plumbing::blob::builtin_driver::text::Labels,
    objs::tree::EntryKind,
};
use gix_testtools::TestResult;

use super::{Verdict, check_forced_resolutions, commit_options, compare, open, parse_merges, tree_options};

const DEFAULT_SEED: u64 = 0x6d65_7267_6573_6565;
const DEFAULT_CASES: u64 = 800;
const VARIANTS: &[&str] = &["default", "ours", "theirs", "union", "diff3", "directory-renames-true"];
/// Leaf paths, overlapping as files and directories, including one that isn't valid UTF-8 where `gix` can merge it.
const PATHS: &[&[u8]] = &[
    b"a",
    b"b",
    b"c",
    b"d/a",
    b"d/b",
    b"d/c",
    b"d/e/a",
    b"e/a",
    b"e/b",
    b"r/a",
    b"x/y/z",
    if cfg!(windows) { b"n\xc3\xa9" as &[u8] } else { b"n\xff" },
];
/// Destinations of renamed files and directories.
const DIRECTORIES: &[&[u8]] = &[b"d", b"d/e", b"e", b"r", b"x/y", b"z"];
const WORDS: &[&str] = &["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
const GITLINKS: usize = 3;

#[test]
fn generated_histories_match_git() -> TestResult {
    // `merge-tree` accepts `-X` since Git 2.45.
    if *gix_testtools::GIT_VERSION < (2, 45, 0) {
        return Ok(());
    }
    let seed = env_u64("GIX_MERGE_SEED")?.unwrap_or(DEFAULT_SEED);
    let indices = match env_u64("GIX_MERGE_CASE")? {
        Some(index) => index..index + 1,
        None => 0..env_u64("GIX_MERGE_CASES")?.unwrap_or(DEFAULT_CASES),
    };
    let tmp = gix_testtools::tempfile::TempDir::new()?;
    let root = tmp.path();
    gix_testtools::git(root, "init --quiet")?;
    let repo = gix::open_opts(root, gix::open::Options::isolated())?;
    let empty_tree_id = repo.object_hash().empty_tree();
    let gitlinks = (0..GITLINKS as i64)
        .map(|time| commit(&repo, empty_tree_id, &[], "gitlink", time))
        .collect::<TestResult<Vec<_>>>()?;
    let histories = indices
        .map(|index| History::generate(&repo, seed, index, &gitlinks))
        .collect::<TestResult<Vec<_>>>()?;
    let mut input = String::new();
    for history in &histories {
        writeln!(input, "{} {}", history.ours, history.theirs)?;
    }

    let mut failures = Vec::new();
    let mut known = 0;
    let mut merges = 0;
    for variant in VARIANTS {
        let expected = parse_merges(&git_merges(root, variant, &input)?)?;
        assert_eq!(expected.len(), histories.len(), "{variant}: Git merged every history");
        let repo = open(root, variant)?;
        for (history, expected) in histories.iter().zip(&expected) {
            let (ours, theirs) = (history.ours.to_string(), history.theirs.to_string());
            let labels = Labels {
                ancestor: None,
                current: Some(ours.as_str().into()),
                other: Some(theirs.as_str().into()),
            };
            let sides = [history.ours_tree_id, history.theirs_tree_id];
            let mut record = |api: &str, report: String| {
                failures.push((
                    history.size,
                    format!("GIX_MERGE_CASE={} {variant} {api}", history.index),
                    format!("{variant} {api}: {}\n{report}", history.describe(seed)),
                ));
            };
            let mut check = |api: &str, outcome: &mut gix::merge::tree::Outcome<'_>| -> TestResult {
                match compare(&repo, expected, outcome, sides)? {
                    Verdict::Same => {}
                    Verdict::Known => known += 1,
                    Verdict::Different(report) => record(api, report),
                }
                Ok(())
            };

            merges += 1;
            let mut outcome = repo
                .merge_commits(history.ours, history.theirs, labels, commit_options(&repo, variant)?)?
                .tree_merge;
            check("merge_commits()", &mut outcome)?;
            if let Some(base_tree_id) = history.base_tree_id.filter(|_| *variant != "diff3") {
                merges += 1;
                let mut outcome = repo.merge_trees(
                    base_tree_id,
                    history.ours_tree_id,
                    history.theirs_tree_id,
                    labels,
                    tree_options(&repo, variant)?,
                )?;
                check("merge_trees()", &mut outcome)?;
            }
            if *variant == "default"
                && let Some(report) = check_forced_resolutions(&repo, |options| {
                    Ok(repo
                        .merge_commits(history.ours, history.theirs, labels, options)?
                        .tree_merge)
                })?
            {
                record("forced resolution", report);
            }
        }
    }

    if !failures.is_empty() {
        failures.sort_by_key(|(size, ..)| *size);
        let shown = failures.len().min(25);
        panic!(
            "{} of {merges} merges of generated histories differ from Git, {known} as documented, with seed {seed}:\n{}\n\nThe {shown} smallest:\n\n{}",
            failures.len(),
            failures
                .iter()
                .map(|(_, summary, _)| summary.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            failures[..shown]
                .iter()
                .map(|(.., failure)| failure.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        );
    }
    Ok(())
}

fn env_u64(name: &str) -> TestResult<Option<u64>> {
    Ok(match std::env::var(name) {
        Ok(value) => Some(value.parse()?),
        Err(std::env::VarError::NotPresent) => None,
        Err(err) => return Err(err.into()),
    })
}

/// Run all queued merges of `input` like the fixture of the hand-written corpus does for `variant`.
fn git_merges(root: &Path, variant: &str, input: &str) -> TestResult<Vec<u8>> {
    let attributes = root.join(".git").join("info").join("attributes");
    let mut cmd = gix_testtools::git_command(root);
    match variant {
        "diff3" => {
            cmd.args(["-c", "merge.conflictStyle=diff3"]);
        }
        "directory-renames-true" => {
            cmd.args(["-c", "merge.directoryRenames=true"]);
        }
        "union" => {
            std::fs::create_dir_all(attributes.parent().expect("in info directory"))?;
            std::fs::write(&attributes, "* merge=union\n")?;
        }
        _ => {}
    }
    cmd.args([
        "merge-tree",
        "--stdin",
        "-z",
        "--write-tree",
        "--messages",
        "--allow-unrelated-histories",
    ]);
    if matches!(variant, "ours" | "theirs") {
        cmd.args(["-X", variant]);
    }
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let input = input.to_owned();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output()?;
    writer.join().expect("writing to Git does not panic")?;
    if variant == "union" {
        std::fs::remove_file(&attributes)?;
    }
    if !output.status.success() {
        return Err(format!("{variant}: {cmd:?} failed: {}", output.stderr.as_bstr()).into());
    }
    Ok(output.stdout)
}

fn commit(
    repo: &gix::Repository,
    tree_id: ObjectId,
    parents: &[ObjectId],
    message: &str,
    time: i64,
) -> TestResult<ObjectId> {
    let signature = gix::actor::Signature {
        name: "gitoxide".into(),
        email: "gitoxide@localhost".into(),
        time: gix::date::Time::new(time, 0),
    };
    Ok(repo
        .write_object(gix::objs::Commit {
            tree: tree_id,
            parents: parents.iter().copied().collect(),
            author: signature.clone(),
            committer: signature,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        })?
        .detach())
}

/// A deterministic `splitmix64` generator.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Entry {
    File { executable: bool, data: Vec<u8> },
    Link(BString),
    Gitlink(usize),
}

type Tree = BTreeMap<BString, Entry>;

/// One generated pair of commits to merge, and how it was made.
struct History {
    index: u64,
    shape: &'static str,
    /// The trees from which the merged commits were made, starting with the first shared ancestor if there is one.
    trees: Vec<(&'static str, Tree)>,
    ours: ObjectId,
    theirs: ObjectId,
    ours_tree_id: ObjectId,
    theirs_tree_id: ObjectId,
    /// The tree of the only merge base, if there is exactly one.
    base_tree_id: Option<ObjectId>,
    size: usize,
}

impl History {
    fn generate(repo: &gix::Repository, seed: u64, index: u64, gitlinks: &[ObjectId]) -> TestResult<Self> {
        let mut rng = Rng(seed ^ index.wrapping_mul(0x2545_f491_4f6c_dd1d));
        rng.next_u64();
        let time = |role: i64| index as i64 * 8 + role + GITLINKS as i64;
        let write = |tree: &Tree| write_tree(repo, tree, gitlinks);
        let base = random_tree(&mut rng);
        let mut trees = Vec::new();
        let (shape, ours, theirs, base_tree_id, ours_tree, theirs_tree) = match rng.below(100) {
            0..=79 => {
                let (ours_tree, theirs_tree) = derive_pair(&base, &mut rng);
                let base_tree_id = write(&base)?;
                let base_id = commit(repo, base_tree_id, &[], "base", time(0))?;
                let ours = commit(repo, write(&ours_tree)?, &[base_id], "ours", time(1))?;
                let theirs = commit(repo, write(&theirs_tree)?, &[base_id], "theirs", time(2))?;
                trees.push(("base", base));
                ("fork", ours, theirs, Some(base_tree_id), ours_tree, theirs_tree)
            }
            80..=94 => {
                let three = rng.chance(33);
                let base_id = commit(repo, write(&base)?, &[], "base", time(0))?;
                let mut bases = Vec::new();
                for (role, name) in [(1, "X"), (2, "Y"), (3, "Z")]
                    .into_iter()
                    .take(if three { 3 } else { 2 })
                {
                    let mut tree = base.clone();
                    mutate(&mut tree, &mut rng, name);
                    bases.push((commit(repo, write(&tree)?, &[base_id], name, time(role))?, name, tree));
                }
                let ours_tree = derive(&bases[0].2, &mut rng, "ours");
                let theirs_tree = derive(&bases[1].2, &mut rng, "theirs");
                let ids: Vec<_> = bases.iter().map(|(id, ..)| *id).collect();
                let reversed: Vec<_> = ids.iter().rev().copied().collect();
                let ours = commit(repo, write(&ours_tree)?, &ids, "ours", time(4))?;
                let theirs = commit(repo, write(&theirs_tree)?, &reversed, "theirs", time(5))?;
                trees.push(("base", base));
                trees.extend(bases.into_iter().map(|(_, name, tree)| (name, tree)));
                (
                    if three { "three merge bases" } else { "criss-cross" },
                    ours,
                    theirs,
                    None,
                    ours_tree,
                    theirs_tree,
                )
            }
            _ => {
                let ours_tree = derive(&base, &mut rng, "ours");
                let theirs_tree = derive(&random_tree(&mut rng), &mut rng, "theirs");
                let ours = commit(repo, write(&ours_tree)?, &[], "ours", time(1))?;
                let theirs = commit(repo, write(&theirs_tree)?, &[], "theirs", time(2))?;
                ("unrelated", ours, theirs, None, ours_tree, theirs_tree)
            }
        };
        let ours_tree_id = write(&ours_tree)?;
        let theirs_tree_id = write(&theirs_tree)?;
        trees.push(("ours", ours_tree));
        trees.push(("theirs", theirs_tree));
        Ok(History {
            index,
            shape,
            size: trees.iter().map(|(_, tree)| tree.len()).sum(),
            trees,
            ours,
            theirs,
            ours_tree_id,
            theirs_tree_id,
            base_tree_id,
        })
    }

    fn describe(&self, seed: u64) -> String {
        let mut out = format!(
            "GIX_MERGE_SEED={seed} GIX_MERGE_CASE={} ({}, ours {}, theirs {})",
            self.index, self.shape, self.ours, self.theirs
        );
        for (name, tree) in &self.trees {
            write!(out, "\n  {name}:").expect("infallible");
            for (path, entry) in tree {
                let entry = match entry {
                    Entry::File { executable, data } => {
                        format!("{} {:?}", if *executable { "100755" } else { "100644" }, data.as_bstr())
                    }
                    Entry::Link(target) => format!("120000 {target:?}"),
                    Entry::Gitlink(index) => format!("160000 gitlink {index}"),
                };
                write!(out, "\n    {path:?} {entry}").expect("infallible");
            }
        }
        out
    }
}

fn write_tree(repo: &gix::Repository, tree: &Tree, gitlinks: &[ObjectId]) -> TestResult<ObjectId> {
    let mut editor = repo.edit_tree(repo.object_hash().empty_tree())?;
    for (path, entry) in tree {
        let (kind, id) = match entry {
            Entry::File { executable, data } => (
                if *executable {
                    EntryKind::BlobExecutable
                } else {
                    EntryKind::Blob
                },
                repo.write_blob(data)?.detach(),
            ),
            Entry::Link(target) => (EntryKind::Link, repo.write_blob(target)?.detach()),
            Entry::Gitlink(index) => (EntryKind::Commit, gitlinks[*index]),
        };
        editor.upsert(path, kind, id)?;
    }
    Ok(editor.write()?.detach())
}

fn random_tree(rng: &mut Rng) -> Tree {
    let mut tree = Tree::new();
    for _ in 0..1 + rng.below(5) {
        let entry = random_entry(rng);
        insert(&mut tree, rng.pick(PATHS).as_bstr().to_owned(), entry);
    }
    tree
}

fn random_entry(rng: &mut Rng) -> Entry {
    match rng.below(20) {
        0 => Entry::Link(rng.pick(PATHS).as_bstr().to_owned()),
        1 => Entry::Gitlink(rng.below(GITLINKS)),
        2 => Entry::File {
            executable: true,
            data: random_content(rng),
        },
        _ => Entry::File {
            executable: false,
            data: random_content(rng),
        },
    }
}

/// Produce numbered lines of one word, sometimes empty, binary, with CRLF or mixed endings, or without a final newline.
fn random_content(rng: &mut Rng) -> Vec<u8> {
    let word = *rng.pick(WORDS);
    let mut out = Vec::new();
    let style = rng.below(20);
    if style == 0 {
        return out;
    }
    if style == 1 {
        out.extend_from_slice(b"binary\0");
    }
    for line in 0..4 + rng.below(6) {
        let eol: &[u8] = match style {
            2 => b"\r\n",
            3 if line % 2 == 1 => b"\r\n",
            _ => b"\n",
        };
        out.extend_from_slice(format!("{word} {line}").as_bytes());
        out.extend_from_slice(eol);
    }
    if style == 4 {
        out.pop();
    }
    out
}

/// Derive both sides from `base`, sometimes applying identical changes to both of them first.
fn derive_pair(base: &Tree, rng: &mut Rng) -> (Tree, Tree) {
    let (mut ours, mut theirs) = (base.clone(), base.clone());
    if rng.chance(25) {
        let shared_seed = rng.next_u64();
        for tree in [&mut ours, &mut theirs] {
            mutate(tree, &mut Rng(shared_seed), "same");
        }
    }
    (derive(&ours, rng, "ours"), derive(&theirs, rng, "theirs"))
}

fn derive(base: &Tree, rng: &mut Rng, side: &str) -> Tree {
    let mut tree = base.clone();
    for _ in 0..1 + rng.below(3) {
        mutate(&mut tree, rng, side);
    }
    tree
}

/// Insert `entry` at `path`, replacing files at its parent paths and everything below it.
fn insert(tree: &mut Tree, path: BString, entry: Entry) {
    for (pos, byte) in path.iter().enumerate() {
        if *byte == b'/' {
            tree.remove(path[..pos].as_bstr());
        }
    }
    let mut directory = path.clone();
    directory.push(b'/');
    tree.retain(|existing, _| !existing.starts_with(&directory));
    tree.insert(path, entry);
}

fn directories(tree: &Tree) -> Vec<BString> {
    let mut out: Vec<BString> = tree
        .keys()
        .flat_map(|path| {
            path.iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'/')
                .map(|(pos, _)| path[..pos].as_bstr().to_owned())
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Apply one random change, with new lines named after `side` so both sides can make identical changes too.
fn mutate(tree: &mut Tree, rng: &mut Rng, side: &str) {
    let paths: Vec<BString> = tree.keys().cloned().collect();
    if paths.is_empty() {
        let entry = random_entry(rng);
        insert(tree, rng.pick(PATHS).as_bstr().to_owned(), entry);
        return;
    }
    let path = rng.pick(&paths).clone();
    let token = if rng.chance(25) { "same" } else { side }.to_owned();
    match rng.below(100) {
        0..=34 => edit(tree, &path, rng, &token),
        35..=44 => {
            tree.remove(&path);
        }
        45..=59 => {
            let destination = if rng.chance(50) {
                rng.pick(PATHS).as_bstr().to_owned()
            } else {
                let name = path.rsplit_str("/").next().expect("non-empty path");
                let mut destination = BString::from(*rng.pick(DIRECTORIES));
                destination.push(b'/');
                destination.extend_from_slice(name);
                destination
            };
            if destination != path {
                let entry = tree.remove(&path).expect("picked from tree");
                insert(tree, destination.clone(), entry);
                if rng.chance(50) {
                    edit(tree, &destination, rng, &token);
                }
            }
        }
        60..=69 => {
            let entry = random_entry(rng);
            insert(tree, rng.pick(PATHS).as_bstr().to_owned(), entry);
        }
        70..=74 => {
            if let Some(Entry::File { executable, .. }) = tree.get_mut(&path) {
                *executable = !*executable;
            }
        }
        75..=79 => {
            let entry = match tree[&path] {
                Entry::File { .. } if rng.chance(70) => Entry::Link(rng.pick(PATHS).as_bstr().to_owned()),
                Entry::File { .. } => Entry::Gitlink(rng.below(GITLINKS)),
                Entry::Link(_) | Entry::Gitlink(_) => Entry::File {
                    executable: false,
                    data: random_content(rng),
                },
            };
            tree.insert(path, entry);
        }
        80..=84 => {
            let entry = tree.remove(&path).expect("picked from tree");
            let mut child = path;
            child.extend_from_slice(b"/n");
            insert(tree, child, entry);
        }
        85..=89 => {
            if let Some(directory) = directories(tree).first().cloned() {
                let entry = random_entry(rng);
                insert(tree, directory, entry);
            }
        }
        90..=94 => {
            let candidates = directories(tree);
            if candidates.is_empty() {
                return;
            }
            let source = rng.pick(&candidates).clone();
            let destination = BString::from(*rng.pick(DIRECTORIES));
            let nested = |a: &BStr, b: &BStr| a == b || (a.starts_with(b) && a.get(b.len()) == Some(&b'/'));
            if nested(source.as_bstr(), destination.as_bstr()) || nested(destination.as_bstr(), source.as_bstr()) {
                return;
            }
            let moved: Vec<_> = paths
                .iter()
                .filter(|path| path.starts_with(&source) && path.get(source.len()) == Some(&b'/'))
                .cloned()
                .collect();
            for path in moved {
                let entry = tree.remove(&path).expect("listed");
                let mut new_path = destination.clone();
                new_path.extend_from_slice(&path[source.len()..]);
                insert(tree, new_path, entry);
            }
        }
        _ => match tree[&path].clone() {
            Entry::Link(_) => {
                tree.insert(path, Entry::Link(format!("{token}-target").into()));
            }
            Entry::Gitlink(index) => {
                tree.insert(path, Entry::Gitlink((index + 1 + rng.below(GITLINKS - 1)) % GITLINKS));
            }
            Entry::File { .. } => edit(tree, &path, rng, &token),
        },
    }
}

/// Replace, insert or remove one line of the file at `path`.
fn edit(tree: &mut Tree, path: &BString, rng: &mut Rng, token: &str) {
    let Some(Entry::File { data, .. }) = tree.get_mut(path) else {
        return;
    };
    let mut lines: Vec<Vec<u8>> = data
        .split_inclusive(|byte| *byte == b'\n')
        .map(ToOwned::to_owned)
        .collect();
    let line = rng.below(lines.len() + 1);
    let eol: &[u8] = match lines.get(line).or(lines.last()) {
        Some(existing) if existing.ends_with(b"\r\n") => b"\r\n",
        _ => b"\n",
    };
    let mut new_line = format!("{token} {line}").into_bytes();
    new_line.extend_from_slice(eol);
    match rng.below(3) {
        0 if line < lines.len() => {
            lines.remove(line);
        }
        1 => lines.insert(line, new_line),
        _ if line < lines.len() => lines[line] = new_line,
        _ => lines.push(new_line),
    }
    *data = lines.concat();
}
