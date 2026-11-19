//! Git's `core.fsync`, `core.fsyncMethod` and `core.fsyncObjectFiles`, which decide what is flushed to storage
//! before it becomes visible.
//!
//! Each written part of the repository belongs to a [component](Components), and [`Settings::mode()`] tells how
//! to flush it:
//!
//! | Component | Written by | Flushed |
//! |-----------|------------|---------|
//! | `loose-object` | everything that writes objects | the object file before it is moved into place, then its fan-out directory, and `objects/` if that directory is new |
//! | `pack` | fetches | the pack file before it is moved into place, then `objects/pack/` |
//! | `pack-metadata` | fetches | the pack index before it is moved into place after the pack, then `objects/pack/` |
//! | `reference` | reference transactions | each loose reference and `packed-refs` before it replaces its predecessor, then its directory |
//! | `index` | status, clone and worktree checkouts, and `gix_index::File::write()` with [`Repository::fsync_settings()`](crate::Repository::fsync_settings()) | the new index before it replaces the old one, then its directory |
//! | `commit-graph`, `object-map` | nothing, as gitoxide doesn't write these files | - |
//!
//! Like Git, nothing is flushed for reflogs, configuration files, the shallow file, or files created when initializing
//! a repository or adding a worktree.
//!
//! Each step finishes flushing before the next one starts, so within one operation, objects and packs are durable
//! before references naming them are written.
//!
//! Directories are flushed on Unix only. `batch` is treated as `fsync`, and `writeout-only` differs from `fsync` only
//! on Apple platforms, see [`gix_fs::Fsync`].
//!
//! ### Full durability
//!
//! To flush everything whatever the repository configuration says, open it with configuration overrides, which take
//! precedence over all configuration files:
//!
//! ```
//! let options = gix::open::Options::default().config_overrides(["core.fsync=all", "core.fsyncMethod=fsync"]);
//! # let _ = options;
//! ```
use gix_error::bail;

use crate::{
    Result,
    bstr::{BStr, ByteSlice},
    config,
    config::tree::{Core, Key},
};

/// A set of the parts of a repository that `core.fsync` can harden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Components(u8);

impl Components {
    /// No component.
    pub const NONE: Self = Self(0);
    /// Objects written as loose files.
    pub const LOOSE_OBJECT: Self = Self(1 << 0);
    /// Pack files.
    pub const PACK: Self = Self(1 << 1);
    /// Pack indices and bitmaps.
    pub const PACK_METADATA: Self = Self(1 << 2);
    /// The commit-graph file.
    pub const COMMIT_GRAPH: Self = Self(1 << 3);
    /// The index file.
    pub const INDEX: Self = Self(1 << 4);
    /// Loose references and `packed-refs`.
    pub const REFERENCE: Self = Self(1 << 5);
    /// The object map of repositories which translate between hash kinds.
    pub const OBJECT_MAP: Self = Self(1 << 6);

    /// The `objects` aggregate: `loose-object` and `pack`.
    pub const OBJECTS: Self = Self::LOOSE_OBJECT.union(Self::PACK);
    /// The `derived-metadata` aggregate: `pack-metadata` and `commit-graph`.
    pub const DERIVED_METADATA: Self = Self::PACK_METADATA.union(Self::COMMIT_GRAPH);
    /// The `committed` aggregate: `objects` and `reference`.
    ///
    /// Git's documentation says it equals `objects`, but Git includes `reference`.
    pub const COMMITTED: Self = Self::OBJECTS.union(Self::REFERENCE);
    /// The `added` aggregate: `committed` and `index`.
    pub const ADDED: Self = Self::COMMITTED.union(Self::INDEX);
    /// The `all` aggregate.
    pub const ALL: Self = Self::ADDED.union(Self::DERIVED_METADATA).union(Self::OBJECT_MAP);
    /// What Git hardens if `core.fsync` isn't set: `pack`, `pack-metadata` and `commit-graph`, and on Windows `loose-object` too.
    pub const PLATFORM_DEFAULT: Self = {
        let default = Self::OBJECTS.union(Self::DERIVED_METADATA);
        if cfg!(windows) {
            default
        } else {
            default.difference(Self::LOOSE_OBJECT)
        }
    };

    /// The names Git accepts, with what they stand for.
    const NAMES: &'static [(&'static str, Self)] = &[
        ("loose-object", Self::LOOSE_OBJECT),
        ("pack", Self::PACK),
        ("pack-metadata", Self::PACK_METADATA),
        ("commit-graph", Self::COMMIT_GRAPH),
        ("index", Self::INDEX),
        ("objects", Self::OBJECTS),
        ("reference", Self::REFERENCE),
        ("derived-metadata", Self::DERIVED_METADATA),
        ("committed", Self::COMMITTED),
        ("added", Self::ADDED),
        ("all", Self::ALL),
    ];

    /// Return the components in either `self` or `other`.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Return the components in `self` but not in `other`.
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Return `true` if all components of `other` are in `self`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Parse a `core.fsync` `value` exactly like Git does.
    ///
    /// Starting from [`PLATFORM_DEFAULT`](Self::PLATFORM_DEFAULT), comma-separated names add components and names
    /// prefixed with `-` remove them. `none` as last item clears the starting set, and an empty value keeps it.
    /// Like in Git, a name selects every component it is a prefix of, so `pack` also selects `pack-metadata`,
    /// unknown names are ignored, and an item that is just `-` ends parsing.
    pub fn from_value(value: &BStr) -> Self {
        let mut current = Self::PLATFORM_DEFAULT;
        let (mut positive, mut negative) = (Self::NONE, Self::NONE);
        let mut rest = value.as_bytes();
        loop {
            rest = &rest[rest.iter().take_while(|b| b", \t\n\r".contains(*b)).count()..];
            let (mut name, next) = rest.split_at(rest.find_byte(b',').unwrap_or(rest.len()));
            rest = next;
            if name == b"none" && next.is_empty() {
                current = Self::NONE;
                continue;
            }
            let negated = name.first() == Some(&b'-');
            if negated {
                name = &name[1..];
                if name.is_empty() {
                    gix_trace::warn!("invalid value for variable core.fsync");
                }
            }
            if name.is_empty() {
                break;
            }
            let mut found = false;
            for (_, bits) in Self::NAMES
                .iter()
                .filter(|(known, _)| known.as_bytes().starts_with(name))
            {
                found = true;
                if negated {
                    negative = negative.union(*bits);
                } else {
                    positive = positive.union(*bits);
                }
            }
            if !found {
                gix_trace::warn!("ignoring unknown core.fsync component '{}'", name.as_bstr());
            }
        }
        current.difference(negative).union(positive)
    }
}

/// The strategy `core.fsyncMethod` selects to harden components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// `fsync`: flush through to storage, [`gix_fs::Fsync::Full`].
    Fsync,
    /// `writeout-only`: write out of the operating system's cache only, [`gix_fs::Fsync::WriteoutOnly`].
    WriteoutOnly,
    /// `batch`: Git flushes loose objects of one operation together. This is done like [`Method::Fsync`].
    Batch,
}

impl Method {
    /// What Git uses if `core.fsyncMethod` isn't set: `writeout-only` on Apple platforms, `batch` on Windows,
    /// and `fsync` everywhere else.
    pub const PLATFORM_DEFAULT: Self = if cfg!(target_vendor = "apple") {
        Method::WriteoutOnly
    } else if cfg!(windows) {
        Method::Batch
    } else {
        Method::Fsync
    };

    /// Parse a `core.fsyncMethod` `value`, or return `None` for values Git ignores.
    pub fn from_value(value: &BStr) -> Option<Self> {
        Some(match value.as_bytes() {
            b"fsync" => Method::Fsync,
            b"writeout-only" => Method::WriteoutOnly,
            b"batch" => Method::Batch,
            _ => return None,
        })
    }
}

/// What to flush, and how, as resolved from `core.fsync`, `core.fsyncMethod` and `core.fsyncObjectFiles`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Settings {
    /// The components to flush.
    pub components: Components,
    /// How to flush them.
    pub method: Method,
}

impl Default for Settings {
    /// Git's defaults for the current platform.
    fn default() -> Self {
        Settings {
            components: Components::PLATFORM_DEFAULT,
            method: Method::PLATFORM_DEFAULT,
        }
    }
}

impl Settings {
    /// Return how to flush data of `component`, which is [`gix_fs::Fsync::Off`] unless it is in our components.
    pub fn mode(&self, component: Components) -> gix_fs::Fsync {
        if !self.components.contains(component) {
            return gix_fs::Fsync::Off;
        }
        match self.method {
            Method::Fsync | Method::Batch => gix_fs::Fsync::Full,
            Method::WriteoutOnly => gix_fs::Fsync::WriteoutOnly,
        }
    }
}

/// Resolve [`Settings`] from all `core` sections of `config` that pass `filter`, in order, like Git does.
///
/// A key without value is an error, unknown values are ignored.
pub(crate) fn settings(config: &gix_config::File, filter: fn(&gix_config::file::Metadata) -> bool) -> Result<Settings> {
    let mut settings = Settings::default();
    for section in config
        .sections_by_name_and_filter("core", filter)
        .into_iter()
        .flatten()
        .filter(|section| section.header().subsection_name().is_none())
    {
        if section.value_implicit(Core::FSYNC.name()) == Some(None) {
            bail!(config::key::error(&Core::FSYNC, "A value is required"));
        }
        if let Some(value) = section.values(Core::FSYNC.name()).last() {
            settings.components = Components::from_value(value.as_bstr());
        }
        if section.value_implicit(Core::FSYNC_METHOD.name()) == Some(None) {
            bail!(config::key::error(&Core::FSYNC_METHOD, "A value is required"));
        }
        for value in section.values(Core::FSYNC_METHOD.name()) {
            match Method::from_value(value.as_bstr()) {
                Some(method) => settings.method = method,
                None => gix_trace::warn!("ignoring unknown core.fsyncMethod value '{}'", value.as_bstr()),
            }
        }
    }
    if Core::FSYNC_OBJECT_FILES.enrich_error(config.boolean_filter(Core::FSYNC_OBJECT_FILES, filter))? == Some(true) {
        settings.components = settings.components.union(Components::LOOSE_OBJECT);
    }
    Ok(settings)
}
