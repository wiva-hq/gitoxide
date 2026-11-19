//! Native [Git LFS](https://git-lfs.com) support, working without the `git-lfs` program.
//!
//! With the `lfs` feature, the `lfs` filter driver is built into every filter pipeline the repository creates:
//! cleaning stores content in the [local store](Platform::store()) and yields its pointer, and smudging yields the
//! object, downloading it on demand with the `blocking-http-transport-reqwest` and `parallel` features. An object that can't be obtained leaves its
//! pointer in the worktree with a warning, which [`Platform::worktree_file()`] can detect.
//! Set `gitoxide.core.builtinLfsFilter` to `false` to use a configured `filter.lfs.*` program instead.
//!
//! [`Repository::lfs()`] lists pointers, fetches objects, and uploads the objects of commits about to be pushed.
//!
//! What isn't supported is listed in the [`plumbing`] crate.
use std::{collections::HashSet, sync::atomic::AtomicBool};

use gix_hash::ObjectId;
pub use gix_lfs as plumbing;
use gix_lfs::{Pointer, pointer::MAX_SIZE};

use crate::{
    Progress, Repository, Result,
    bstr::{BStr, BString, ByteVec},
    error::{OptionExt, ResultExt, message},
};

mod config;
pub(crate) mod driver;

/// A platform to work with LFS objects of a repository, created by [`Repository::lfs()`].
#[derive(Clone, Copy)]
pub struct Platform<'repo> {
    /// The repository to work with.
    pub repo: &'repo Repository,
}

/// A pointer found in a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerEntry {
    /// The path of the pointer file relative to the root of the tree it was found in.
    pub path: BString,
    /// The id of the blob containing the pointer.
    pub blob_id: ObjectId,
    /// The pointer.
    pub pointer: Pointer,
}

/// The state of a file in the worktree, as determined by [`Platform::worktree_file()`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeFile {
    /// There is no file at the path.
    Absent,
    /// The file isn't a pointer, as expected after a checkout with the object being available.
    NotAPointer,
    /// The file is a pointer, typically because its object wasn't available during checkout.
    Pointer {
        /// The pointer in the file.
        pointer: Pointer,
        /// If `true`, the object is in the local store, so checking the file out again yields its content.
        object_in_store: bool,
    },
}

/// Listing pointers
impl Platform<'_> {
    /// List all pointers in the tree with `tree_id` and its subtrees, in tree order.
    ///
    /// Blobs smaller than 1024 bytes that parse as pointer are listed, whether or not attributes assign the `lfs`
    /// filter to them, as `git lfs ls-files` does.
    pub fn pointers_in_tree(&self, tree_id: impl Into<ObjectId>) -> Result<Vec<PointerEntry>> {
        let mut out = Vec::new();
        self.collect_pointers(tree_id.into(), BString::default(), None, Some(&mut out))?;
        Ok(out)
    }

    /// List the pointers in the trees of the commits reachable from `tips` but not from `hidden`, each one once.
    ///
    /// Trees and blobs that are reachable from the trees of the `hidden` commits themselves are skipped, so with
    /// `hidden` being the tips the remote has, these are the pointers introduced by the commits to send.
    pub fn pointers_in_commits(
        &self,
        tips: impl IntoIterator<Item = impl Into<ObjectId>>,
        hidden: impl IntoIterator<Item = impl Into<ObjectId>>,
    ) -> Result<Vec<PointerEntry>> {
        let hidden: Vec<ObjectId> = hidden.into_iter().map(Into::into).collect();
        let mut seen = HashSet::new();
        for commit_id in &hidden {
            let tree_id = self.repo.find_commit(*commit_id)?.tree_id()?.detach();
            if seen.insert(tree_id) {
                self.collect_pointers(tree_id, BString::default(), Some(&mut seen), None)?;
            }
        }
        let mut out = Vec::new();
        for info in self.repo.rev_walk(tips).with_hidden(hidden).all()? {
            let info = info?;
            let tree_id = self.repo.find_commit(info.id)?.tree_id()?.detach();
            if seen.insert(tree_id) {
                self.collect_pointers(tree_id, BString::default(), Some(&mut seen), Some(&mut out))?;
            }
        }
        Ok(out)
    }

    /// Collect pointers of the tree with `tree_id` into `out` with `prefix` prepended to their paths.
    /// If `seen` is set, trees and blobs in it are skipped, and all visited ones are added to it.
    fn collect_pointers(
        &self,
        tree_id: ObjectId,
        prefix: BString,
        mut seen: Option<&mut HashSet<ObjectId>>,
        mut out: Option<&mut Vec<PointerEntry>>,
    ) -> Result {
        let tree = self.repo.find_tree(tree_id)?;
        for entry in tree.decode()?.entries {
            let id = entry.oid.to_owned();
            if seen.as_mut().is_some_and(|seen| !seen.insert(id)) {
                continue;
            }
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push_byte(b'/');
            }
            path.push_str(entry.filename);
            if entry.mode.is_tree() {
                self.collect_pointers(id, path, seen.as_deref_mut(), out.as_deref_mut())?;
            } else if entry.mode.is_blob()
                && let Some(out) = out.as_deref_mut()
                && let Some(pointer) = self.pointer_in_blob(id)?
            {
                out.push(PointerEntry {
                    path,
                    blob_id: id,
                    pointer,
                });
            }
        }
        Ok(())
    }

    /// Return the pointer stored in the blob with `blob_id`, or `None` if it isn't a pointer to a non-empty object.
    pub fn pointer_in_blob(&self, blob_id: impl Into<ObjectId>) -> Result<Option<Pointer>> {
        let blob_id = blob_id.into();
        let size = self.repo.find_header(blob_id)?.size();
        if size == 0 || size >= MAX_SIZE as u64 {
            return Ok(None);
        }
        let blob = self.repo.find_blob(blob_id)?;
        Ok(Pointer::from_bytes(&blob.data).ok())
    }

    /// Determine whether the worktree file at `rela_path` is a pointer, and if so, whether its object is available.
    pub fn worktree_file(&self, rela_path: &BStr) -> Result<WorktreeFile> {
        use std::io::Read;
        let workdir = self
            .repo
            .workdir()
            .ok_or_raise(|| message("A bare repository has no worktree files").validation())?;
        let path = workdir.join(gix_path::from_bstr(rela_path)?);
        let file = match std::fs::symlink_metadata(&path) {
            Ok(md) if md.is_file() => std::fs::File::open(&path),
            Ok(_) => return Ok(WorktreeFile::NotAPointer),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(WorktreeFile::Absent),
            Err(err) => Err(err),
        };
        let mut head = Vec::new();
        file.and_then(|file| file.take(MAX_SIZE as u64).read_to_end(&mut head))
            .or_raise(|| message!("Could not read worktree file '{}'", path.display()))?;
        Ok(match Pointer::from_bytes(&head) {
            Ok(pointer) if pointer.size > 0 => WorktreeFile::Pointer {
                object_in_store: self.store()?.contains(&pointer),
                pointer,
            },
            _ => WorktreeFile::NotAPointer,
        })
    }
}

/// Transfers
impl Platform<'_> {
    /// Download the objects of `pointers` that aren't in the local store yet with `client`.
    ///
    /// See [`gix_lfs::Client::download()`] for details.
    pub fn fetch(
        &self,
        client: &gix_lfs::Client,
        pointers: &[Pointer],
        progress: &mut dyn Progress,
        interrupt: &AtomicBool,
    ) -> Result<gix_lfs::client::Outcome> {
        client.download(&self.store()?, pointers, progress, interrupt)
    }

    /// Upload the objects of the pointers in commits reachable from `tips` but not from `hidden` with `client`,
    /// as needed before pushing `tips` to a remote which has `hidden`.
    ///
    /// Objects the server already has are skipped. See [`gix_lfs::Client::upload()`] for details.
    pub fn upload_for_push(
        &self,
        client: &gix_lfs::Client,
        tips: impl IntoIterator<Item = impl Into<ObjectId>>,
        hidden: impl IntoIterator<Item = impl Into<ObjectId>>,
        progress: &mut dyn Progress,
        interrupt: &AtomicBool,
    ) -> Result<gix_lfs::client::Outcome> {
        let pointers: Vec<_> = self
            .pointers_in_commits(tips, hidden)?
            .into_iter()
            .map(|entry| entry.pointer)
            .collect();
        client.upload(&self.store()?, &pointers, progress, interrupt)
    }
}
