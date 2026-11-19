use gix::{
    bstr::ByteSlice,
    config::fsync::{Components, Method, Settings},
};
use gix_testtools::TestResult;

fn components(value: &str) -> Components {
    Components::from_value(value.as_bytes().as_bstr())
}

#[test]
fn components_match_the_table_in_gits_documentation() {
    for (name, expected) in [
        ("loose-object", Components::LOOSE_OBJECT),
        ("pack", Components::PACK.union(Components::PACK_METADATA)),
        ("pack-metadata", Components::PACK_METADATA),
        ("commit-graph", Components::COMMIT_GRAPH),
        ("index", Components::INDEX),
        ("objects", Components::LOOSE_OBJECT.union(Components::PACK)),
        ("reference", Components::REFERENCE),
        (
            "derived-metadata",
            Components::PACK_METADATA.union(Components::COMMIT_GRAPH),
        ),
        ("committed", Components::OBJECTS.union(Components::REFERENCE)),
        ("added", Components::COMMITTED.union(Components::INDEX)),
        (
            "all",
            Components::ADDED
                .union(Components::DERIVED_METADATA)
                .union(Components::OBJECT_MAP),
        ),
    ] {
        assert_eq!(
            components(&format!("{name},none")),
            expected,
            "{name} selects what Git documents, and every name it is a prefix of, like 'pack' does"
        );
    }
}

#[test]
fn components_parse_like_git() {
    let default = Components::PLATFORM_DEFAULT;
    for (value, expected, why) in [
        ("", default, "an empty value keeps the default"),
        ("none", Components::NONE, "'none' clears everything"),
        (
            "none,index",
            default.union(Components::INDEX),
            "'none' only counts as last item and is an unknown name elsewhere",
        ),
        (
            "index,none",
            Components::INDEX,
            "'none' as last item clears only the default",
        ),
        ("all,-index", Components::ALL, "additions win over removals"),
        (
            "-pack",
            default.difference(Components::PACK.union(Components::PACK_METADATA)),
            "removals apply to the default",
        ),
        (
            "commit",
            default.union(Components::COMMIT_GRAPH).union(Components::COMMITTED),
            "a prefix selects all names it starts",
        ),
        (
            " index , reference",
            default.union(Components::REFERENCE),
            "leading whitespace is skipped, but trailing whitespace makes a name unknown",
        ),
        (
            "index,,reference",
            default.union(Components::INDEX).union(Components::REFERENCE),
            "repeated separators are skipped",
        ),
        ("-,index", default, "a lone '-' ends parsing"),
        ("bogus", default, "unknown names are ignored"),
    ] {
        assert_eq!(components(value), expected, "{value:?}: {why}");
    }
    assert_eq!(
        Components::PLATFORM_DEFAULT,
        if cfg!(windows) {
            Components::OBJECTS.union(Components::DERIVED_METADATA)
        } else {
            Components::PACK.union(Components::DERIVED_METADATA)
        },
        "Git hardens packs and derived metadata by default, and loose objects too on Windows"
    );
}

#[test]
fn method_parses_like_git_and_selects_the_flush() {
    for (value, expected) in [
        ("fsync", Some(Method::Fsync)),
        ("writeout-only", Some(Method::WriteoutOnly)),
        ("batch", Some(Method::Batch)),
        ("FSYNC", None),
        ("", None),
    ] {
        assert_eq!(
            Method::from_value(value.as_bytes().as_bstr()),
            expected,
            "{value:?}: only exact values are known"
        );
    }
    assert_eq!(
        Method::PLATFORM_DEFAULT,
        if cfg!(target_vendor = "apple") {
            Method::WriteoutOnly
        } else if cfg!(windows) {
            Method::Batch
        } else {
            Method::Fsync
        },
        "Git's default method depends on the platform"
    );

    for (method, expected) in [
        (Method::Fsync, gix_fs::Fsync::Full),
        (Method::Batch, gix_fs::Fsync::Full),
        (Method::WriteoutOnly, gix_fs::Fsync::WriteoutOnly),
    ] {
        let settings = Settings {
            components: Components::INDEX.union(Components::LOOSE_OBJECT),
            method,
        };
        assert_eq!(
            settings.mode(Components::INDEX),
            expected,
            "{method:?} flushes enabled components"
        );
        assert_eq!(
            settings.mode(Components::REFERENCE),
            gix_fs::Fsync::Off,
            "{method:?} leaves other components alone"
        );
        assert_eq!(
            settings.mode(Components::OBJECTS),
            gix_fs::Fsync::Off,
            "an aggregate is only enabled if all of its components are"
        );
    }
}

fn open(overrides: &[&str]) -> gix::Result<gix::Repository> {
    gix::open_opts(
        gix_testtools::scripted_fixture_read_only("make_basic_repo.sh").expect("the fixture can be created"),
        gix::open::Options::isolated()
            .strict_config(true)
            .config_overrides(overrides.iter().copied()),
    )
}

#[test]
fn settings_resolve_from_configuration_like_git() -> TestResult {
    assert_eq!(
        open(&[])?.fsync_settings(),
        Settings::default(),
        "Git's defaults apply if nothing is set"
    );
    for (overrides, expected, why) in [
        (
            &["core.fsync=index", "core.fsync=reference"][..],
            Settings {
                components: Components::PLATFORM_DEFAULT.union(Components::REFERENCE),
                ..Default::default()
            },
            "the last value wins, and each starts from the default",
        ),
        (
            &["core.fsyncMethod=writeout-only", "core.fsyncMethod=bogus"][..],
            Settings {
                method: Method::WriteoutOnly,
                ..Default::default()
            },
            "unknown methods are ignored",
        ),
        (
            &["core.fsync=none", "core.fsyncObjectFiles=true"][..],
            Settings {
                components: Components::LOOSE_OBJECT,
                ..Default::default()
            },
            "the deprecated core.fsyncObjectFiles adds loose objects",
        ),
    ] {
        assert_eq!(open(overrides)?.fsync_settings(), expected, "{overrides:?}: {why}");
    }
    for key in ["core.fsync", "core.fsyncMethod"] {
        assert!(open(&[key]).is_err(), "{key} without value is an error, as in Git");
    }
    Ok(())
}

#[test]
fn writes_flush_objects_and_references_as_configured() -> TestResult {
    for (fsync, expected_files) in [("none", 0), ("all", 1)] {
        let (repo, _keep) = crate::repo_rw_opts(
            "make_basic_repo.sh",
            gix::open::Options::isolated().config_overrides([
                "user.name=gitoxide",
                "user.email=gitoxide@localhost",
                format!("core.fsync={fsync}").as_str(),
                "core.fsyncMethod=fsync",
            ]),
        )?;

        let before = gix_fs::flush_count();
        let blob_id = repo.write_blob(format!("core.fsync={fsync}"))?;
        assert_eq!(
            gix_fs::flush_count().files - before.files,
            expected_files,
            "core.fsync={fsync}: a new loose object is flushed if configured"
        );

        let before = gix_fs::flush_count();
        repo.reference(
            "refs/heads/fsync",
            blob_id,
            gix::refs::transaction::PreviousValue::MustNotExist,
            "create",
        )?;
        assert_eq!(
            gix_fs::flush_count().files - before.files,
            expected_files,
            "core.fsync={fsync}: the new reference is flushed if configured, but not its reflog"
        );
    }
    Ok(())
}
