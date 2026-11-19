use std::io::Read;

use gix_lfs::{
    Pointer, Store,
    filter::{Smudged, clean, smudge},
};

use crate::content_pointer;

#[test]
fn clean_stores_content_and_smudge_restores_it() -> gix_error::TestResult {
    let tmp = gix_testtools::tempfile::TempDir::new()?;
    let store = Store::at(tmp.path().join("lfs"));
    let content = b"large content\n".repeat(1000);

    let pointer_bytes = clean(&store, &mut content.as_slice())?;
    let pointer = content_pointer(&content);
    assert_eq!(pointer_bytes, pointer.to_bytes(), "the canonical pointer is returned");
    assert!(store.contains(&pointer), "the content is stored");
    assert!(
        store.object_path(&pointer.oid).starts_with(
            store
                .root()
                .join("objects")
                .join(&pointer.oid.to_hex()[..2])
                .join(&pointer.oid.to_hex()[2..4])
        ),
        "objects are fanned out by the first two bytes of their id"
    );
    assert_eq!(
        std::fs::read_dir(store.root().join("tmp"))?.count(),
        0,
        "no temporary files remain"
    );

    assert_eq!(
        clean(&store, &mut pointer_bytes.as_slice())?,
        pointer_bytes,
        "pointers are kept as they are"
    );
    assert_eq!(
        clean(&store, &mut content.as_slice())?,
        pointer_bytes,
        "storing again is fine"
    );

    let Smudged::Object(mut file) = smudge(&store, &pointer_bytes)? else {
        panic!("the object is present")
    };
    let mut smudged = Vec::new();
    file.read_to_end(&mut smudged)?;
    assert_eq!(smudged, content, "smudging restores the content");

    assert!(matches!(smudge(&store, b"not a pointer")?, Smudged::NotAPointer));
    assert!(matches!(smudge(&store, b"")?, Smudged::Empty));
    let missing = content_pointer(b"not stored");
    assert!(
        matches!(smudge(&store, &missing.to_bytes())?, Smudged::Missing(pointer) if pointer == missing),
        "missing objects are reported with their pointer"
    );
    Ok(())
}

#[test]
fn small_content_is_cleaned_too() -> gix_error::TestResult {
    let tmp = gix_testtools::tempfile::TempDir::new()?;
    let store = Store::at(tmp.path());
    let pointer = Pointer::from_bytes(&clean(&store, &mut &b"hi\n"[..])?)?;
    assert_eq!(
        pointer,
        content_pointer(b"hi\n"),
        "content smaller than a pointer is stored as well"
    );
    assert!(
        clean(&store, &mut &b""[..])?.is_empty(),
        "empty content is the empty pointer, and stays empty"
    );
    Ok(())
}

#[test]
fn pointers_with_extensions_are_not_smudged() -> gix_error::TestResult {
    let tmp = gix_testtools::tempfile::TempDir::new()?;
    let store = Store::at(tmp.path());
    let mut pointer = content_pointer(b"content");
    pointer.extensions.push(gix_lfs::pointer::Extension {
        priority: 0,
        name: "foo".into(),
        oid: gix_lfs::Oid::for_content(b"original"),
    });
    let err = smudge(&store, &pointer.to_bytes()).expect_err("extensions aren't supported");
    assert!(err.is_unsupported(), "the error says that this isn't supported");
    Ok(())
}

#[test]
fn layout_and_pointer_match_git_lfs() -> gix_testtools::Result {
    let fixture = gix_testtools::scripted_fixture_read_only("make_git_lfs_store.sh")?;
    let content = std::fs::read(fixture.join("content"))?;
    let tmp = gix_testtools::tempfile::TempDir::new()?;
    let store = Store::at(tmp.path());
    let pointer_bytes = clean(&store, &mut content.as_slice())?;
    let pointer = Pointer::from_bytes(&pointer_bytes)?;
    assert_eq!(pointer, content_pointer(&content), "the pointer describes the content");

    if fixture.join("git-lfs-unavailable").exists() {
        eprintln!("Skipping the comparison with git-lfs as it isn't installed");
        return Ok(());
    }
    assert_eq!(
        std::fs::read(fixture.join("pointer"))?,
        pointer_bytes,
        "git-lfs writes the same pointer"
    );
    let ours = store.object_path(&pointer.oid);
    let git_lfs_store = Store::at(fixture.join("repo").join(".git").join("lfs"));
    let theirs = git_lfs_store.object_path(&pointer.oid);
    assert_eq!(
        ours.strip_prefix(store.root())?,
        theirs.strip_prefix(git_lfs_store.root())?,
        "objects are stored at the same location relative to the store"
    );
    assert_eq!(std::fs::read(theirs)?, content, "git-lfs stored the object there");
    Ok(())
}
