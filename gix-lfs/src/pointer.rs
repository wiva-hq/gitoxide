use std::fmt::Write;

use bstr::{BString, ByteSlice};
use gix_error::{ErrorExt, Result, bail, message};

use crate::Oid;

/// A pointer is smaller than this many bytes, so larger content is never a pointer.
pub const MAX_SIZE: usize = 1024;

/// The version written into each pointer.
pub const VERSION: &str = "https://git-lfs.github.com/spec/v1";

/// Versions of pre-release tools, accepted when reading.
const LEGACY_VERSIONS: [&str; 2] = ["https://hawser.github.com/spec/v1", "http://git-media.io/v/2"];

/// The text file stored in Git in place of the content of a large file.
///
/// The empty file is the pointer to the empty object, and is kept as is by `clean` and `smudge`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pointer {
    /// The id of the object.
    pub oid: Oid,
    /// The size of the object in bytes.
    pub size: u64,
    /// The extensions that were applied to the content, ordered by priority.
    pub extensions: Vec<Extension>,
}

/// An `ext-<priority>-<name>` line, recording the content an extension received when cleaning.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Extension {
    /// The position of the extension in the pipeline, from 0 to 9.
    pub priority: u8,
    /// The name of the extension.
    pub name: BString,
    /// The id of the content the extension received.
    pub oid: Oid,
}

/// Lifecycle
impl Pointer {
    /// Create a pointer to the object `oid` with `size` bytes, without extensions.
    pub fn new(oid: Oid, size: u64) -> Self {
        Pointer {
            oid,
            size,
            extensions: Vec::new(),
        }
    }

    /// The pointer to the empty object.
    pub fn empty() -> Self {
        Pointer::new(Oid::empty(), 0)
    }

    /// Parse `data` as a pointer, failing with a validation error if it isn't one.
    ///
    /// Like `git-lfs`, surrounding whitespace, `\r\n` line endings and legacy version identifiers are accepted,
    /// extension lines may appear anywhere before `size`, and keys other than `version`, `oid`, `size` and
    /// extensions are refused.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() >= MAX_SIZE {
            bail!(gix_error::validation(
                "Content of 1024 bytes or more is not an LFS pointer"
            ));
        }
        if data.is_empty() {
            return Ok(Pointer::empty());
        }
        let text = data.trim_ascii();
        if text.is_empty() {
            bail!(gix_error::validation("Whitespace is not an LFS pointer"));
        }

        const KEYS: [&str; 3] = ["version", "oid", "size"];
        let mut seen = 0;
        let mut oid = None;
        let mut size = None;
        let mut extensions = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once_str(b" ") else {
                bail!(
                    gix_error::validation("An LFS pointer line must be a key and a value separated by a space")
                        .with_input(line)
                );
            };
            let Some(&expected) = KEYS.get(seen) else {
                bail!(gix_error::validation("An LFS pointer must end with its 'size' line").with_input(line));
            };
            if key == expected.as_bytes() {
                match seen {
                    0 => {
                        let version = value
                            .to_str()
                            .ok()
                            .filter(|version| *version == VERSION || LEGACY_VERSIONS.contains(version));
                        if version.is_none() {
                            bail!(gix_error::validation("Unsupported LFS pointer version").with_input(value));
                        }
                    }
                    1 => oid = Some(parse_oid(value)?),
                    _ => size = Some(parse_size(value)?),
                }
                seen += 1;
            } else if let Some(extension) = parse_extension(key, value)? {
                extensions.push(extension);
            } else {
                bail!(
                    message!(
                        "Expected the '{expected}' key in an LFS pointer, found '{}'",
                        key.as_bstr()
                    )
                    .validation()
                );
            }
        }
        let (Some(oid), Some(size)) = (oid, size) else {
            bail!(gix_error::validation(
                "An LFS pointer needs the 'version', 'oid' and 'size' keys"
            ));
        };
        extensions.sort_by_key(|extension: &Extension| extension.priority);
        if let Some(pair) = extensions.windows(2).find(|pair| pair[0].priority == pair[1].priority) {
            bail!(
                message!(
                    "LFS pointer extensions must have distinct priorities, found {} twice",
                    pair[0].priority
                )
                .validation()
            );
        }
        Ok(Pointer { oid, size, extensions })
    }
}

/// Serialization
impl Pointer {
    /// Return the canonical form of this pointer, the only one `git-lfs` writes.
    ///
    /// Pointers to empty objects are written as empty file.
    pub fn to_bytes(&self) -> Vec<u8> {
        if self.size == 0 {
            return Vec::new();
        }
        let mut out = format!("version {VERSION}\n");
        for extension in &self.extensions {
            writeln!(
                out,
                "ext-{}-{} sha256:{}",
                extension.priority, extension.name, extension.oid
            )
            .expect("writing to a String never fails");
        }
        writeln!(out, "oid sha256:{}\nsize {}", self.oid, self.size).expect("writing to a String never fails");
        out.into_bytes()
    }
}

fn parse_oid(value: &[u8]) -> Result<Oid> {
    let Some(hex) = value.strip_prefix(b"sha256:") else {
        bail!(gix_error::validation("An LFS object id must use the 'sha256' hash method").with_input(value));
    };
    Oid::from_hex(hex)
}

fn parse_size(value: &[u8]) -> Result<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        bail!(gix_error::validation("An LFS object size must be a decimal number").with_input(value));
    }
    value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|size| u64::try_from(size).ok())
        .ok_or_else(|| {
            gix_error::validation("An LFS object size is too large")
                .with_input(value)
                .raise()
        })
}

/// Parse `key` as `ext-<digit>-<name>`, or return `None` if it isn't an extension key.
fn parse_extension(key: &[u8], value: &[u8]) -> Result<Option<Extension>> {
    let Some(rest) = key.strip_prefix(b"ext-") else {
        return Ok(None);
    };
    let (priority, name) = match rest {
        [digit @ b'0'..=b'9', b'-', name @ ..]
            if name.first().is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                && name.iter().all(u8::is_ascii_graphic) =>
        {
            (digit - b'0', name)
        }
        _ => return Ok(None),
    };
    Ok(Some(Extension {
        priority,
        name: name.into(),
        oid: parse_oid(value)?,
    }))
}
