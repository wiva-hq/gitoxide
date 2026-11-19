use gix_lfs::{Oid, Pointer, pointer::Extension};

const SPEC_OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";

fn oid(hex: &str) -> Oid {
    Oid::from_hex(hex.as_bytes()).expect("valid hex in tests")
}

#[test]
fn the_example_of_the_specification_round_trips() -> gix_error::TestResult {
    let canonical = "version https://git-lfs.github.com/spec/v1
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393
size 12345
";
    let pointer = Pointer::from_bytes(canonical.as_bytes())?;
    assert_eq!(pointer, Pointer::new(oid(SPEC_OID), 12345), "all fields are parsed");
    assert_eq!(
        pointer.to_bytes(),
        canonical.as_bytes(),
        "the canonical form is reproduced exactly"
    );
    Ok(())
}

#[test]
fn extensions_are_kept_and_written_in_priority_order() -> gix_error::TestResult {
    let foo = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let bar = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let input = format!(
        "version https://git-lfs.github.com/spec/v1
ext-1-bar sha256:{bar}
ext-0-foo sha256:{foo}
oid sha256:{SPEC_OID}
size 12345
"
    );
    let pointer = Pointer::from_bytes(input.as_bytes())?;
    assert_eq!(
        pointer.extensions,
        [
            Extension {
                priority: 0,
                name: "foo".into(),
                oid: oid(foo),
            },
            Extension {
                priority: 1,
                name: "bar".into(),
                oid: oid(bar),
            }
        ],
        "extensions are sorted by priority"
    );
    assert_eq!(
        pointer.to_bytes(),
        format!(
            "version https://git-lfs.github.com/spec/v1
ext-0-foo sha256:{foo}
ext-1-bar sha256:{bar}
oid sha256:{SPEC_OID}
size 12345
"
        )
        .as_bytes(),
        "extensions are written between version and oid"
    );

    let duplicate = format!(
        "version https://git-lfs.github.com/spec/v1
ext-0-foo sha256:{foo}
ext-0-bar sha256:{bar}
oid sha256:{SPEC_OID}
size 12345
"
    );
    assert!(
        Pointer::from_bytes(duplicate.as_bytes()).is_err(),
        "priorities must be unique"
    );
    Ok(())
}

#[test]
fn lenient_forms_written_by_other_tools_are_accepted() -> gix_error::TestResult {
    let expected = Pointer::new(oid(SPEC_OID), 12345);
    for input in [
        format!("version https://hawser.github.com/spec/v1\noid sha256:{SPEC_OID}\nsize 12345\n"),
        format!("version http://git-media.io/v/2\noid sha256:{SPEC_OID}\nsize 12345\n"),
        format!("version https://git-lfs.github.com/spec/v1\r\noid sha256:{SPEC_OID}\r\nsize 12345\r\n"),
        format!("\n  version https://git-lfs.github.com/spec/v1\noid sha256:{SPEC_OID}\nsize 12345"),
    ] {
        assert_eq!(
            Pointer::from_bytes(input.as_bytes())?,
            expected,
            "{input:?} is a pointer"
        );
    }
    assert_eq!(
        Pointer::from_bytes(b"")?,
        Pointer::empty(),
        "empty content is the pointer to the empty object"
    );
    assert!(Pointer::empty().to_bytes().is_empty(), "which is written as empty file");
    Ok(())
}

#[test]
fn invalid_pointers_are_refused() {
    let large = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{SPEC_OID}\nsize 12345\n{}",
        " ".repeat(1024)
    );
    for (input, reason) in [
        (
            format!("oid sha256:{SPEC_OID}\nversion https://git-lfs.github.com/spec/v1\nsize 1\n"),
            "keys out of order",
        ),
        (
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{SPEC_OID}\n"),
            "size missing",
        ),
        (
            format!("version https://git-lfs.github.com/spec/v2\noid sha256:{SPEC_OID}\nsize 1\n"),
            "unknown version",
        ),
        (
            format!("version https://git-lfs.github.com/spec/v1\noid sha1:{SPEC_OID}\nsize 1\n"),
            "unknown hash",
        ),
        (
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 1\n",
                SPEC_OID.to_uppercase()
            ),
            "uppercase hex",
        ),
        (
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{SPEC_OID}\nsize -1\n"),
            "negative size",
        ),
        (
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{SPEC_OID}\nsize +1\n"),
            "signed size",
        ),
        (
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{SPEC_OID}\nsize 1\nextra 1\n"),
            "keys after size",
        ),
        (
            format!("version https://git-lfs.github.com/spec/v1\nfoo bar\noid sha256:{SPEC_OID}\nsize 1\n"),
            "unknown keys",
        ),
        (large, "1024 bytes or more"),
        ("   \n".into(), "only whitespace"),
        ("hello world\n".into(), "arbitrary text"),
    ] {
        let err = Pointer::from_bytes(input.as_bytes()).expect_err(reason);
        assert!(err.is_validation(), "{reason}: errors are validation errors");
    }
}
