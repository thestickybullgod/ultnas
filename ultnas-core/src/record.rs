//! Record lifecycle: creation, sealing, and verification.

use crate::{address::hash_bytes, ContentId, NamespacePath, UltnasCoreError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single archived record in the Ultnas vault.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// BLAKE3 hash of the record's canonical content bytes.
    pub id: ContentId,
    /// When this record was first created.
    pub created_at: DateTime<Utc>,
    /// The namespace this record belongs to.
    pub namespace: NamespacePath,
    /// Human-readable label (not unique within a namespace).
    pub label: String,
    /// Searchable tags.
    pub tags: Vec<String>,
    /// MIME type of the content.
    pub media_type: String,
    /// Content size in bytes.
    pub size_bytes: u64,
    /// Cryptographic seal, set once the record is sealed.
    pub seal: Option<RecordSeal>,
    /// Arbitrary key–value metadata.
    pub metadata: BTreeMap<String, String>,
}

/// Cryptographic seal attached to a record after sealing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordSeal {
    /// When this record was sealed.
    pub sealed_at: DateTime<Utc>,
    /// Hex-encoded Ed25519 public key of the sealer.
    pub sealed_by: String,
    /// Hex-encoded Ed25519 signature over the record's canonical bytes.
    pub signature: String,
    /// ContentId of the policy file in effect at seal time.
    pub policy_hash: ContentId,
}

impl Record {
    /// Returns `true` if this record has been sealed.
    pub fn is_sealed(&self) -> bool {
        self.seal.is_some()
    }

    /// Seal this record.
    ///
    /// # Errors
    /// Returns [`UltnasCoreError::AlreadySealed`] if the record is already sealed.
    pub fn seal_record(
        &mut self,
        sealed_by_pubkey_hex: String,
        signature_hex: String,
        policy_hash: ContentId,
    ) -> Result<(), UltnasCoreError> {
        if self.is_sealed() {
            return Err(UltnasCoreError::AlreadySealed);
        }
        self.seal = Some(RecordSeal {
            sealed_at: Utc::now(),
            sealed_by: sealed_by_pubkey_hex,
            signature: signature_hex,
            policy_hash,
        });
        Ok(())
    }

    /// Verify that `content` matches this record's ContentId.
    ///
    /// # Errors
    /// Returns [`UltnasCoreError::IntegrityFailure`] on mismatch.
    pub fn verify_content(&self, content: &[u8]) -> Result<(), UltnasCoreError> {
        let actual = hash_bytes(content);
        if actual != self.id {
            return Err(UltnasCoreError::IntegrityFailure {
                expected: self.id.to_hex(),
                actual: actual.to_hex(),
            });
        }
        Ok(())
    }

    /// Return a deterministic canonical serialization used for hashing and signing.
    /// Uses fully owned fields — no lifetime tricks, no memory leaks.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        #[derive(Serialize)]
        struct CanonicalRecord {
            id: String,         // ContentId hex
            created_at: String, // ISO 8601
            namespace: String,  // slash-joined path
            label: String,
            tags: Vec<String>,
            media_type: String,
            size_bytes: u64,
            metadata: BTreeMap<String, String>,
        }

        let canonical = CanonicalRecord {
            id: self.id.to_hex(),
            created_at: self.created_at.to_rfc3339(),
            namespace: self.namespace.as_str(),
            label: self.label.clone(),
            tags: self.tags.clone(),
            media_type: self.media_type.clone(),
            size_bytes: self.size_bytes,
            metadata: self.metadata.clone(),
        };

        serde_json::to_vec(&canonical).expect("canonical serialization is infallible")
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Fluent builder for constructing a [`Record`] before writing it to the vault.
pub struct RecordBuilder {
    namespace: NamespacePath,
    label: String,
    tags: Vec<String>,
    media_type: String,
    metadata: BTreeMap<String, String>,
}

impl RecordBuilder {
    /// Begin building a record in `namespace` with a given `label`.
    pub fn new(namespace: NamespacePath, label: impl Into<String>) -> Self {
        Self {
            namespace,
            label: label.into(),
            tags: vec![],
            media_type: "application/octet-stream".to_string(),
            metadata: BTreeMap::new(),
        }
    }

    /// Append a tag.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Set the MIME type.
    pub fn media_type(mut self, mt: impl Into<String>) -> Self {
        self.media_type = mt.into();
        self
    }

    /// Add an arbitrary metadata key–value pair.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Finalise the builder. Hashes `content` and assigns a [`ContentId`].
    pub fn build(self, content: &[u8]) -> Result<Record, UltnasCoreError> {
        let id = hash_bytes(content);
        Ok(Record {
            id,
            created_at: Utc::now(),
            namespace: self.namespace,
            label: self.label,
            tags: self.tags,
            media_type: self.media_type,
            size_bytes: content.len() as u64,
            seal: None,
            metadata: self.metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(content: &[u8]) -> Record {
        let ns = NamespacePath::parse("test/records").unwrap();
        RecordBuilder::new(ns, "test-doc")
            .tag("test")
            .media_type("text/plain")
            .build(content)
            .unwrap()
    }

    #[test]
    fn build_creates_correct_id() {
        let content = b"hello ultnas";
        let r = make_record(content);
        assert_eq!(r.id, hash_bytes(content));
        assert!(!r.is_sealed());
    }

    #[test]
    fn verify_content_ok() {
        let content = b"test content";
        let r = make_record(content);
        assert!(r.verify_content(content).is_ok());
    }

    #[test]
    fn verify_content_fails_on_tamper() {
        let content = b"original";
        let r = make_record(content);
        assert!(r.verify_content(b"tampered").is_err());
    }

    #[test]
    fn canonical_bytes_are_deterministic() {
        let content = b"canonical test";
        let r = make_record(content);
        assert_eq!(r.canonical_bytes(), r.canonical_bytes());
    }

    #[test]
    fn double_seal_returns_error() {
        let content = b"seal test";
        let mut r = make_record(content);
        let dummy_hash = hash_bytes(b"policy");
        r.seal_record("pubkey".to_string(), "sig".to_string(), dummy_hash)
            .unwrap();
        let result = r.seal_record("pubkey".to_string(), "sig".to_string(), dummy_hash);
        assert!(matches!(result, Err(UltnasCoreError::AlreadySealed)));
    }
}
