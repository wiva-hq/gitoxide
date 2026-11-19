use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use gix_error::{ErrorExt, Result, ResultExt, bail, message};

use crate::{Oid, Pointer, oid::Hasher};

/// The local object store of Git LFS, typically at `<git-common-dir>/lfs`.
///
/// It uses the layout of `git-lfs`, so both can share it: objects live at `objects/<oid[0..2]>/<oid[2..4]>/<oid>`,
/// new objects are written to `tmp/` first, and partial downloads are kept in `incomplete/<oid>.part` to be resumed.
/// Objects are only ever added after their content was hashed and found to match the id they are stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    root: PathBuf,
}

/// Lifecycle
impl Store {
    /// Use the store at `root`, which doesn't have to exist yet.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }
}

/// Access
impl Store {
    /// The directory containing the store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path at which the object with `oid` is stored, whether it exists or not.
    pub fn object_path(&self, oid: &Oid) -> PathBuf {
        let hex = oid.to_hex();
        self.root.join("objects").join(&hex[..2]).join(&hex[2..4]).join(hex)
    }

    /// Return `true` if the object `pointer` refers to is present with the expected size.
    pub fn contains(&self, pointer: &Pointer) -> bool {
        std::fs::metadata(self.object_path(&pointer.oid)).is_ok_and(|md| md.is_file() && md.len() == pointer.size)
    }

    /// Open the object `pointer` refers to for reading, or return `None` if it isn't present with the expected size.
    pub fn open(&self, pointer: &Pointer) -> Result<Option<File>> {
        let path = self.object_path(&pointer.oid);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => bail!(err.and_raise(message!("Could not open LFS object at '{}'", path.display()))),
        };
        let size = file
            .metadata()
            .or_raise(|| message!("Could not read metadata of LFS object at '{}'", path.display()))?
            .len();
        Ok((size == pointer.size).then_some(file))
    }
}

/// Modification
impl Store {
    /// Stream all of `src` into a new object and return the pointer to it.
    ///
    /// The data is written to a temporary file while being hashed, synced, and moved into place unless
    /// the object already exists.
    pub fn write_from(&self, src: &mut dyn Read) -> Result<Pointer> {
        let tmp_dir = self.root.join("tmp");
        std::fs::create_dir_all(&tmp_dir)
            .or_raise(|| message!("Could not create directory '{}'", tmp_dir.display()))?;
        let mut tempfile = new_tempfile(&tmp_dir)?;
        let mut hasher = Hasher::default();
        let mut size = 0u64;
        let mut buf = vec![0; 64 * 1024];
        loop {
            let read = match src.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => read,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => bail!(err.and_raise(message("Could not read the content to store as LFS object"))),
            };
            hasher.update(&buf[..read]);
            tempfile
                .write_all(&buf[..read])
                .or_raise(|| message!("Could not write to a temporary file in '{}'", tmp_dir.display()))?;
            size += read as u64;
        }
        let pointer = Pointer::new(hasher.finalize(), size);
        if self.contains(&pointer) {
            return Ok(pointer);
        }
        tempfile
            .with_mut(|file| file.as_file().sync_all())
            .and_then(std::convert::identity)
            .or_raise(|| message("Could not sync a new LFS object to disk"))?;
        let path = self.create_object_dir(&pointer.oid)?;
        if let Err(err) = tempfile.persist(&path)
            && !self.contains(&pointer)
        {
            bail!(
                err.error
                    .and_raise(message!("Could not move a new LFS object to '{}'", path.display()))
            );
        }
        Ok(pointer)
    }

    fn create_object_dir(&self, oid: &Oid) -> Result<PathBuf> {
        let path = self.object_path(oid);
        let dir = path.parent().expect("objects are stored in a directory");
        std::fs::create_dir_all(dir).or_raise(|| message!("Could not create directory '{}'", dir.display()))?;
        Ok(path)
    }

    /// Open the partial download of the object `pointer` refers to, to continue where it left off.
    pub(crate) fn partial(&self, pointer: &Pointer) -> Result<Partial> {
        let dir = self.root.join("incomplete");
        std::fs::create_dir_all(&dir).or_raise(|| message!("Could not create directory '{}'", dir.display()))?;
        let path = dir.join(format!("{}.part", pointer.oid));
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .or_raise(|| message!("Could not open partial download at '{}'", path.display()))?;
        let mut hasher = Hasher::default();
        let mut len = 0u64;
        let mut buf = vec![0; 64 * 1024];
        loop {
            let read = file
                .read(&mut buf)
                .or_raise(|| message!("Could not read partial download at '{}'", path.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            len += read as u64;
        }
        let mut partial = Partial {
            file,
            path,
            hasher,
            len,
        };
        if partial.len >= pointer.size {
            partial.restart()?;
        }
        Ok(partial)
    }
}

/// A download in progress, kept in `incomplete/<oid>.part` so it can be resumed after a failure.
pub(crate) struct Partial {
    file: File,
    path: PathBuf,
    hasher: Hasher,
    len: u64,
}

impl Partial {
    /// The amount of bytes already downloaded.
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Discard all downloaded bytes.
    pub(crate) fn restart(&mut self) -> Result {
        self.file
            .set_len(0)
            .and_then(|()| self.file.seek(SeekFrom::Start(0)).map(|_| ()))
            .or_raise(|| message!("Could not truncate partial download at '{}'", self.path.display()))?;
        self.hasher = Hasher::default();
        self.len = 0;
        Ok(())
    }

    /// Append `bytes` to the download.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result {
        self.file
            .write_all(bytes)
            .or_raise(|| message!("Could not write partial download at '{}'", self.path.display()))?;
        self.hasher.update(bytes);
        self.len += bytes.len() as u64;
        Ok(())
    }

    /// Verify the downloaded content against `pointer` and move it into `store`.
    ///
    /// On mismatch, the download is discarded and an error classified as corruption is returned.
    pub(crate) fn finish(self, store: &Store, pointer: &Pointer) -> Result {
        let Partial {
            file,
            path,
            hasher,
            len,
        } = self;
        let oid = hasher.finalize();
        if len != pointer.size || oid != pointer.oid {
            drop(file);
            std::fs::remove_file(&path).ok();
            bail!(
                message!(
                    "Downloaded LFS object has {len} bytes with id {oid}, expected {} bytes with id {}",
                    pointer.size,
                    pointer.oid
                )
                .corrupted()
            );
        }
        file.sync_all()
            .or_raise(|| message!("Could not sync downloaded LFS object at '{}'", path.display()))?;
        drop(file);
        let destination = store.create_object_dir(&pointer.oid)?;
        if let Err(err) = std::fs::rename(&path, &destination) {
            if !store.contains(pointer) {
                bail!(err.and_raise(message!(
                    "Could not move downloaded LFS object to '{}'",
                    destination.display()
                )));
            }
            std::fs::remove_file(&path).ok();
        }
        Ok(())
    }
}

fn new_tempfile(dir: &Path) -> Result<gix_tempfile::Handle<gix_tempfile::handle::Writable>> {
    #[cfg(unix)]
    let tempfile = {
        use std::os::unix::fs::PermissionsExt;
        gix_tempfile::new_with_permissions(
            dir,
            gix_tempfile::ContainingDirectory::Exists,
            gix_tempfile::AutoRemove::Tempfile,
            std::fs::Permissions::from_mode(0o666),
        )
    };
    #[cfg(not(unix))]
    let tempfile = gix_tempfile::new(
        dir,
        gix_tempfile::ContainingDirectory::Exists,
        gix_tempfile::AutoRemove::Tempfile,
    );
    tempfile.or_raise(|| message!("Could not create a temporary file in '{}'", dir.display()))
}
