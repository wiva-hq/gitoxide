use std::{cell::Cell, fs::File, io, path::Path};

/// How to make written data durable before it becomes visible under its final name.
///
/// This is what Git's `core.fsyncMethod` selects for each part of the repository that `core.fsync` enables.
/// [`Off`](Fsync::Off) performs no system call at all.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fsync {
    /// Don't flush. Data reaches storage whenever the operating system decides and may be lost on a crash or power loss.
    #[default]
    Off,
    /// Write data out of the operating system's cache without forcing the storage device to empty its own cache.
    ///
    /// This is Git's `writeout-only`. It differs from [`Full`](Fsync::Full) only on Apple platforms, where it uses
    /// `fsync()` instead of `fcntl(F_FULLFSYNC)`. On all other platforms it is the same as `Full`.
    WriteoutOnly,
    /// Flush data through all caches to permanent storage.
    ///
    /// Apple platforms use `fcntl(F_FULLFSYNC)` and fall back to `fsync()` on file systems which don't support it,
    /// other Unix systems use `fsync()`, and Windows uses `FlushFileBuffers()`.
    Full,
}

/// Flushes the current thread issued through [`Fsync`] so far, counted whether they succeeded or not.
///
/// Tests use the difference between two readings to learn which write paths flush.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlushCount {
    /// The number of file flushes.
    pub files: usize,
    /// The number of directory flushes. These don't happen on Windows.
    pub directories: usize,
}

thread_local! {
    static FILES: Cell<usize> = const { Cell::new(0) };
    static DIRECTORIES: Cell<usize> = const { Cell::new(0) };
}

/// Return the flushes the current thread issued so far.
pub fn flush_count() -> FlushCount {
    FlushCount {
        files: FILES.with(Cell::get),
        directories: DIRECTORIES.with(Cell::get),
    }
}

impl Fsync {
    /// Return `true` unless this is [`Fsync::Off`].
    pub fn is_enabled(self) -> bool {
        self != Fsync::Off
    }

    /// Flush the contents and metadata of `file` to storage.
    ///
    /// On Windows, `file` must have been opened with write access.
    pub fn file(self, file: &File) -> io::Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        FILES.with(|count| count.set(count.get() + 1));
        flush(file, self)
    }

    /// Flush the directory at `path` so that entries created, renamed or removed in it survive a crash.
    ///
    /// A rename isn't durable before its directory is flushed on Unix. On Windows this does nothing as directories
    /// can't be flushed there. File systems which reject flushing directories are ignored.
    pub fn directory(self, path: &Path) -> io::Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            DIRECTORIES.with(|count| count.set(count.get() + 1));
            let path = if path.as_os_str().is_empty() {
                Path::new(".")
            } else {
                path
            };
            match flush(&File::open(path)?, self) {
                Err(err) if matches!(err.kind(), io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported) => Ok(()),
                res => res,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(())
        }
    }

    /// Flush the directory containing `path`, after `path` was created or renamed into place.
    pub fn parent_directory(self, path: &Path) -> io::Result<()> {
        self.directory(path.parent().unwrap_or(Path::new("")))
    }
}

#[cfg(target_vendor = "apple")]
fn flush(file: &File, mode: Fsync) -> io::Result<()> {
    fn retry_interrupted(f: impl Fn() -> rustix::io::Result<()>) -> rustix::io::Result<()> {
        loop {
            match f() {
                Err(err) if err == rustix::io::Errno::INTR => continue,
                res => return res,
            }
        }
    }
    if mode == Fsync::Full && retry_interrupted(|| rustix::fs::fcntl_fullfsync(file)).is_ok() {
        return Ok(());
    }
    retry_interrupted(|| rustix::fs::fsync(file)).map_err(io::Error::from)
}

#[cfg(not(target_vendor = "apple"))]
fn flush(file: &File, _mode: Fsync) -> io::Result<()> {
    file.sync_all()
}
