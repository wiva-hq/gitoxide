use std::fmt;

use gix_error::{Error, ErrorExt, Result};

/// The identifier of an LFS object, the SHA-256 hash of its content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid([u8; 32]);

impl Oid {
    /// Create an instance from the raw bytes of a hash.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Oid(bytes)
    }

    /// Parse `hex`, which must be 64 lowercase hexadecimal characters as the specification requires.
    pub fn from_hex(hex: &[u8]) -> Result<Self> {
        fn nibble(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            }
        }
        let invalid =
            || gix_error::validation("An LFS object id must be 64 lowercase hexadecimal characters").with_input(hex);
        if hex.len() != 64 {
            return Err(invalid().raise());
        }
        let mut bytes = [0u8; 32];
        for (byte, [hi, lo]) in bytes.iter_mut().zip(hex.as_chunks::<2>().0) {
            let (Some(hi), Some(lo)) = (nibble(*hi), nibble(*lo)) else {
                return Err(invalid().raise());
            };
            *byte = (hi << 4) | lo;
        }
        Ok(Oid(bytes))
    }

    /// Compute the id of `content`.
    pub fn for_content(content: &[u8]) -> Self {
        let mut hasher = Hasher::default();
        hasher.update(content);
        hasher.finalize()
    }

    /// The id of the empty object.
    pub fn empty() -> Self {
        Oid([
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24, 0x27, 0xae,
            0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
        ])
    }

    /// Return the raw bytes of the hash.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Return the hash as 64 lowercase hexadecimal characters.
    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(char::from(DIGITS[usize::from(byte >> 4)]));
            out.push(char::from(DIGITS[usize::from(byte & 0xf)]));
        }
        out
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.to_hex())
    }
}

impl std::str::FromStr for Oid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Oid::from_hex(s.as_bytes())
    }
}

/// Incrementally compute an [`Oid`].
#[derive(Default, Clone)]
pub(crate) struct Hasher(sha2::Sha256);

impl Hasher {
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        sha2::Digest::update(&mut self.0, bytes);
    }

    pub(crate) fn finalize(self) -> Oid {
        Oid(sha2::Digest::finalize(self.0).into())
    }
}
