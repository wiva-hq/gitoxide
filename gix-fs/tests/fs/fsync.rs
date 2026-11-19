use std::path::Path;

use gix_fs::Fsync;
use gix_testtools::TestResult;

#[test]
fn off_touches_nothing_while_other_modes_flush_files_and_unix_directories() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("file");
    let file = std::fs::File::create(&path)?;

    let before = gix_fs::flush_count();
    Fsync::Off.file(&file)?;
    Fsync::Off.directory(Path::new("does-not-exist"))?;
    assert_eq!(gix_fs::flush_count(), before, "Off doesn't access the file system");

    for mode in [Fsync::WriteoutOnly, Fsync::Full] {
        let before = gix_fs::flush_count();
        mode.file(&file)?;
        mode.parent_directory(&path)?;
        let after = gix_fs::flush_count();
        assert_eq!(after.files - before.files, 1, "{mode:?} flushes the file");
        assert_eq!(
            after.directories - before.directories,
            usize::from(cfg!(unix)),
            "{mode:?} flushes the directory on Unix, where renames need it to be durable"
        );
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn flushing_a_missing_directory_fails() {
    let err = Fsync::Full
        .directory(Path::new("does-not-exist"))
        .expect_err("a directory that doesn't exist can't be flushed");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "the cause is preserved");
}
