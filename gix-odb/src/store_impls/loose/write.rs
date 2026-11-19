use gix_error::Result;
use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

use gix_error::{ErrorExt, Message, ResultExt};
use gix_object::WriteTo;
use gix_zlib::stream::deflate;
use tempfile::NamedTempFile;

use super::Store;
use crate::store_impls::loose;

impl gix_object::Write for Store {
    /// Write failures include [metadata](gix_error::Error::metadata()) `path` (native path), the temporary object
    /// directory.
    fn write(&self, object: &dyn WriteTo) -> Result<gix_hash::ObjectId> {
        let mut to = self.dest()?;
        to.write_all(&object.loose_header())
            .or_raise(|| write_header_error(&self.path))?;
        object.write_to(&mut to).or_raise(|| stream_data_error(&self.path))?;
        to.flush().or_error()?;
        self.finalize_object(to, self.write_policy())
    }

    /// Write the given buffer in `from` to disk in one syscall at best.
    ///
    /// This will cost at least 4 IO operations.
    /// Write failures include [metadata](gix_error::Error::metadata()) `path` (native path), the temporary object
    /// directory.
    fn write_buf(&self, kind: gix_object::Kind, from: &[u8]) -> Result<gix_hash::ObjectId> {
        let mut to = self.dest()?;
        to.write_all(&gix_object::encode::loose_header(kind, from.len() as u64))
            .or_raise(|| write_header_error(&self.path))?;

        to.write_all(from).or_raise(|| stream_data_error(&self.path))?;
        to.flush().or_error()?;
        self.finalize_object(to, self.write_policy())
    }

    /// Write failures include [metadata](gix_error::Error::metadata()) `path` (native path), the temporary object
    /// directory.
    fn write_buf_with_known_id(
        &self,
        kind: gix_object::Kind,
        from: &[u8],
        id: gix_hash::ObjectId,
    ) -> Result<gix_hash::ObjectId> {
        self.write_buf_with_known_id_and_policy(kind, from, id, self.write_policy())
    }

    fn write_stream(&self, kind: gix_object::Kind, size: u64, from: &mut dyn io::Read) -> Result<gix_hash::ObjectId> {
        self.write_stream_with_policy(kind, size, from, self.write_policy())
    }

    fn write_stream_with_known_id(
        &self,
        kind: gix_object::Kind,
        size: u64,
        from: &mut dyn io::Read,
        id: gix_hash::ObjectId,
    ) -> Result<gix_hash::ObjectId> {
        self.write_stream_with_known_id_and_policy(kind, size, from, id, self.write_policy())
    }
}

impl Store {
    pub(crate) fn write_buf_with_known_id_and_policy(
        &self,
        kind: gix_object::Kind,
        from: &[u8],
        id: gix_hash::ObjectId,
        policy: WritePolicy,
    ) -> Result<gix_hash::ObjectId> {
        let mut to = self.compressed_tempfile()?;
        to.write_all(&gix_object::encode::loose_header(kind, from.len() as u64))
            .or_raise(|| write_header_error(&self.path))?;

        to.write_all(from).or_raise(|| stream_data_error(&self.path))?;
        to.flush().or_error()?;
        self.finalize_object_at(id, to, policy)
    }

    /// Write the given stream in `from` to disk with at least one syscall.
    ///
    /// This will cost at least 4 IO operations.
    /// Write failures include [metadata](gix_error::Error::metadata()) `path` (native path), the temporary object
    /// directory.
    pub(crate) fn write_stream_with_policy(
        &self,
        kind: gix_object::Kind,
        size: u64,
        mut from: &mut dyn io::Read,
        policy: WritePolicy,
    ) -> Result<gix_hash::ObjectId> {
        let mut to = self.dest()?;
        to.write_all(&gix_object::encode::loose_header(kind, size))
            .or_raise(|| write_header_error(&self.path))?;

        io::copy(&mut from, &mut to).or_raise(|| stream_data_error(&self.path))?;
        to.flush().or_error()?;
        self.finalize_object(to, policy)
    }

    /// Write failures include [metadata](gix_error::Error::metadata()) `path` (native path), the temporary object
    /// directory.
    pub(crate) fn write_stream_with_known_id_and_policy(
        &self,
        kind: gix_object::Kind,
        size: u64,
        mut from: &mut dyn io::Read,
        id: gix_hash::ObjectId,
        policy: WritePolicy,
    ) -> Result<gix_hash::ObjectId> {
        let mut to = self.compressed_tempfile()?;
        to.write_all(&gix_object::encode::loose_header(kind, size))
            .or_raise(|| write_header_error(&self.path))?;

        io::copy(&mut from, &mut to).or_raise(|| stream_data_error(&self.path))?;
        to.flush().or_error()?;
        self.finalize_object_at(id, to, policy)
    }
}

type CompressedTempfile = deflate::Write<NamedTempFile>;

/// How a new loose object is published, which a dynamic store handle may decide per write.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WritePolicy {
    pub shared_repository_permissions: gix_fs::SharedPermissions,
    pub fsync: gix_fs::Fsync,
}

/// Access
impl Store {
    /// Return the path to the object with `id`.
    ///
    /// Note that is may not exist yet.
    pub fn object_path(&self, id: &gix_hash::oid) -> PathBuf {
        loose::hash_path(id, self.path.clone())
    }
}

impl Store {
    fn write_policy(&self) -> WritePolicy {
        WritePolicy {
            shared_repository_permissions: self.shared_repository_permissions,
            fsync: self.fsync,
        }
    }

    /// A compressed tempfile, with auto-hashing.
    fn dest(&self) -> Result<gix_hash::io::Write<CompressedTempfile>> {
        Ok(gix_hash::io::Write::new(self.compressed_tempfile()?, self.object_hash))
    }

    /// A compressed tempfile, without hasher.
    /// Creation failures include [metadata](gix_error::Error::metadata()) `path` (native path), the temporary object
    /// directory.
    fn compressed_tempfile(&self) -> Result<CompressedTempfile> {
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o444);
            builder.permissions(perms);
        }
        Ok(deflate::Write::new(
            builder.tempfile_in(&self.path).or_raise(|| {
                Message::new("Could not create temporary object file").with("path", self.path.as_path())
            })?,
            self.compression,
        ))
    }

    /// Hashing failures include [metadata](gix_error::Error::metadata()) `path` (native path), the temporary object
    /// directory.
    fn finalize_object(
        &self,
        gix_hash::io::Write { hash, inner: file }: gix_hash::io::Write<CompressedTempfile>,
        policy: WritePolicy,
    ) -> Result<gix_hash::ObjectId> {
        let id = hash
            .try_finalize()
            .or_raise(|| Message::new("Could not hash temporary object file").with("path", self.path.as_path()))?;
        self.finalize_object_at(id, file, policy)
    }

    /// Publication failures include [metadata](gix_error::Error::metadata()) `path` (native path), the object directory
    /// or destination file.
    fn finalize_object_at(
        &self,
        id: gix_hash::ObjectId,
        file: CompressedTempfile,
        policy: WritePolicy,
    ) -> Result<gix_hash::ObjectId> {
        let WritePolicy {
            shared_repository_permissions,
            fsync,
        } = policy;
        let object_path = loose::hash_path(&id, self.path.clone());
        let object_dir = object_path
            .parent()
            .expect("each object path has a 1 hex-bytes directory");
        let created_object_dir = match fs::create_dir(object_dir) {
            Ok(()) => {
                gix_fs::set_shared_repository_permissions(object_dir, shared_repository_permissions)
                    .or_raise(|| Message::new("Could not set object directory permissions").with("path", object_dir))?;
                true
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => false,
            Err(err) => {
                return Err(err.and_raise(Message::new("Could not create object directory").with("path", object_dir)));
            }
        };
        let file = file.into_inner();
        gix_fs::set_shared_repository_permissions(file.path(), shared_repository_permissions)
            .or_raise(|| Message::new("Could not set loose object permissions").with("path", file.path()))?;
        fsync
            .file(file.as_file())
            .or_raise(|| Message::new("Could not flush loose object").with("path", file.path()))?;
        let res = file.persist(&object_path);
        // On windows, we assume that such errors are due to its special filesystem semantics,
        // on any other platform that would be a legitimate error though.
        #[cfg(windows)]
        if let Err(err) = &res
            && (err.error.kind() == io::ErrorKind::PermissionDenied || err.error.kind() == io::ErrorKind::AlreadyExists)
        {
            return Ok(id);
        }
        res.or_raise(|| Message::new("Could not persist loose object").with("path", object_path.as_path()))?;
        fsync
            .directory(object_dir)
            .or_raise(|| Message::new("Could not flush object directory").with("path", object_dir))?;
        if created_object_dir {
            fsync
                .directory(&self.path)
                .or_raise(|| Message::new("Could not flush object directory").with("path", self.path.as_path()))?;
        }
        Ok(id)
    }
}

/// The raised error's [metadata](gix_error::Error::metadata()) `path` (native path) identifies the temporary object
/// directory for a header-write failure.
fn write_header_error(path: &Path) -> Message {
    Message::new("Could not write loose object header").with("path", path)
}

/// The raised error's [metadata](gix_error::Error::metadata()) `path` (native path) identifies the temporary object
/// directory for a data-streaming failure.
fn stream_data_error(path: &Path) -> Message {
    Message::new("Could not stream loose object data").with("path", path)
}
