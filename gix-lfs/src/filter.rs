//! The `clean` and `smudge` operations of the `lfs` filter driver, as `git-lfs` implements them.
use std::{fs::File, io::Read};

use gix_error::{Result, ResultExt, bail, message};

use crate::{Pointer, Store, pointer::MAX_SIZE};

/// Turn the worktree content read from `src` into what Git stores, and return it.
///
/// Content is streamed into `store` and its pointer is returned in canonical form.
/// Content that already is a pointer, including empty content, is returned unchanged and nothing is stored.
pub fn clean(store: &Store, src: &mut dyn Read) -> Result<Vec<u8>> {
    let mut head = Vec::with_capacity(MAX_SIZE);
    (&mut *src)
        .take(MAX_SIZE as u64)
        .read_to_end(&mut head)
        .or_raise(|| message("Could not read the content to clean"))?;
    if head.len() < MAX_SIZE && Pointer::from_bytes(&head).is_ok() {
        return Ok(head);
    }
    let pointer = store.write_from(&mut head.as_slice().chain(src))?;
    Ok(pointer.to_bytes())
}

/// What [`smudge()`] found for its input.
#[derive(Debug)]
pub enum Smudged {
    /// The input isn't a pointer and belongs into the worktree unchanged.
    NotAPointer,
    /// The input is the empty pointer, and the worktree file is empty.
    Empty,
    /// The object the pointer refers to, opened for reading.
    Object(File),
    /// The object the pointer refers to isn't in the store.
    Missing(Pointer),
}

/// Determine what to write into the worktree for `input`, as stored in Git, using objects in `store`.
///
/// Pointers with extensions are refused, as extensions aren't supported.
pub fn smudge(store: &Store, input: &[u8]) -> Result<Smudged> {
    if input.is_empty() {
        return Ok(Smudged::Empty);
    }
    let Ok(pointer) = Pointer::from_bytes(input) else {
        return Ok(Smudged::NotAPointer);
    };
    if let Some(extension) = pointer.extensions.first() {
        bail!(
            message!(
                "LFS pointer to {} uses the extension '{}', but extensions are not supported",
                pointer.oid,
                extension.name
            )
            .unsupported()
        );
    }
    Ok(match store.open(&pointer)? {
        Some(file) => Smudged::Object(file),
        None => Smudged::Missing(pointer),
    })
}
