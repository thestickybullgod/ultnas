//! Content addressing using BLAKE3.
//!
//! All records in an Ultnas vault are identified by their [`ContentId`] — a
//! 32-byte BLAKE3 digest of their canonical content bytes.

use std::{fmt, io::Read, path::Path};

use serde::{Deserialize, Serialize};

use crate::error::UltnasCoreError;

/// A 32-byte BLAKE3 content digest. The canonical identifier for any record.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentId(#[serde(with = "hex_serde")] [u8; 32]);

impl fmt::Debug for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentId({})", self.to_hex())
    }
}

impl fmt::Display for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl ContentId {
    /// Hex-encoded string representation (64 lowercase hex characters).
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse from a 64-character lowercase hex string.
    pub fn from_hex(s: &str) -> Result<Self, UltnasCoreError> {
        let bytes = hex::decode(s).map_err(|_| UltnasCoreError::InvalidContentId(s.to_string()))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| UltnasCoreError::InvalidContentId(s.to_string()))?;
        Ok(Self(arr))
    }

    /// Raw 32-byte array reference.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Create directly from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

// ── Free hashing functions ────────────────────────────────────────────────────

/// Hash arbitrary bytes and return a [`ContentId`].
pub fn hash_bytes(data: &[u8]) -> ContentId {
    let hash = blake3::hash(data);
    ContentId(*hash.as_bytes())
}

/// Hash a file at `path` in streaming fashion (does not load the file into memory).
pub fn hash_file(path: &Path) -> Result<ContentId, UltnasCoreError> {
    let file = std::fs::File::open(path)?;
    hash_reader(&mut std::io::BufReader::new(file))
}

/// Hash any [`Read`] implementor in streaming fashion.
pub fn hash_reader<R: Read>(reader: &mut R) -> Result<ContentId, UltnasCoreError> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(ContentId(*hasher.finalize().as_bytes()))
}

// ── Hex serde helper ─────────────────────────────────────────────────────────
mod hex_serde {
    use serde::{de::Error, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s: &str = serde::Deserialize::deserialize(d)?;
        let v = hex::decode(s).map_err(D::Error::custom)?;
        v.try_into()
            .map_err(|_| D::Error::custom("expected 32 bytes"))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_bytes_is_deterministic() {
        let a = hash_bytes(b"hello ultnas");
        let b = hash_bytes(b"hello ultnas");
        assert_eq!(a, b);
    }

    #[test]
    fn different_content_differs() {
        let a = hash_bytes(b"hello");
        let b = hash_bytes(b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn hex_roundtrip() {
        let id = hash_bytes(b"test content");
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64);
        let id2 = ContentId::from_hex(&hex).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn invalid_hex_returns_error() {
        assert!(ContentId::from_hex("not-valid-hex!!!").is_err());
    }
}
