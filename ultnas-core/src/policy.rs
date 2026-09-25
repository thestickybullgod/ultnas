//! Policy parsing and evaluation engine.

use crate::{address::hash_bytes, ContentId, NamespacePath, Record, UltnasCoreError};
use serde::{Deserialize, Serialize};

/// A fully parsed and validated Ultnas policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub version: u32,
    #[serde(default)]
    pub global: GlobalPolicy,
    #[serde(default)]
    pub namespaces: Vec<NamespacePolicy>,
}

/// The policy used when none is configured: every default, no namespaces.
impl Default for Policy {
    fn default() -> Self {
        Self {
            version: 1,
            global: GlobalPolicy::default(),
            namespaces: vec![],
        }
    }
}

/// Global defaults applied to all namespaces unless overridden.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalPolicy {
    #[serde(default = "default_conflict")]
    pub conflict: ConflictStrategy,
    pub max_record_size_bytes: Option<u64>,
    #[serde(default)]
    pub require_seal_before_rotation: bool,
    /// Integrity guard configuration — controls write-violation detection and restore.
    #[serde(default)]
    pub integrity: IntegrityPolicy,
}

impl Default for GlobalPolicy {
    fn default() -> Self {
        Self {
            conflict: ConflictStrategy::Version,
            max_record_size_bytes: None,
            require_seal_before_rotation: false,
            integrity: IntegrityPolicy::default(),
        }
    }
}

fn default_conflict() -> ConflictStrategy {
    ConflictStrategy::Version
}

/// Configuration for the `IntegrityGuard` service.
///
/// Placed under `[global.integrity]` in the policy TOML file.
///
/// # Example
///
/// ```toml
/// [global.integrity]
/// write_violation_threshold    = 5
/// violation_window_secs        = 300
/// debounce_ms                  = 100
/// restore_source               = "memory_then_store"
/// escalate_after_restores      = 3
/// approval                     = "automatic"   # or "approved"
/// log_each_violation           = true
/// cache_budget_bytes           = 268435456   # 256 MiB
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityPolicy {
    /// Number of write violations against one file within `violation_window_secs`
    /// before a delete-and-restore is triggered. For tracked files, each
    /// violation below this count is prevented by stripping the invisible
    /// characters it introduced. Default: 5.
    #[serde(default = "default_threshold")]
    pub write_violation_threshold: u32,

    /// Sliding window in seconds. Violation counts reset after this much silence.
    /// Default: 300 (5 minutes).
    #[serde(default = "default_window")]
    pub violation_window_secs: u64,

    /// Events arriving within this many milliseconds of the previous event for the
    /// same ContentId are collapsed into a single count increment. Default: 100.
    #[serde(default = "default_debounce")]
    pub debounce_ms: u64,

    /// Restore source priority. Default: `"memory_then_store"`.
    ///
    /// Valid values:
    /// - `"memory"`           — the in-memory VerifiedCache only
    /// - `"store"`            — the vault's copy only
    /// - `"memory_then_store"`— memory, then the vault (default)
    /// - `"remote"`           — reserved; treated as `"memory_then_store"`
    ///
    /// The vault's copy is only independent of a *tracked* file. A sealed
    /// object in the vault is itself the file being restored, so those are
    /// restored from memory whatever this says.
    #[serde(default = "default_restore_source")]
    pub restore_source: String,

    /// How many successful restores of files within one namespace trigger quarantine.
    /// Default: 3.
    #[serde(default = "default_escalate")]
    pub escalate_after_restores: u32,

    /// Whether to write a journal entry for every individual violation
    /// (can be noisy in high-traffic vaults). Default: true.
    #[serde(default = "default_true")]
    pub log_each_violation: bool,

    /// Maximum bytes the VerifiedCache may hold in memory. Default: 256 MiB.
    #[serde(default = "default_cache_budget")]
    pub cache_budget_bytes: u64,

    /// What happens to a clean edit (no invisible characters) of a tracked
    /// file. Overridable per namespace. Default: `"automatic"`.
    #[serde(default)]
    pub approval: ApprovalMode,
}

/// How clean edits to tracked files become the stable (restore) version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// The edit becomes the stable version as soon as it is seen.
    #[default]
    Automatic,
    /// The edit stays on disk as a *pending* version. Restores still use the
    /// last approved version until `ultnas approve <file>` promotes it.
    Approved,
}

impl ApprovalMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalMode::Automatic => "automatic",
            ApprovalMode::Approved => "approved",
        }
    }
}

impl Default for IntegrityPolicy {
    fn default() -> Self {
        Self {
            write_violation_threshold: default_threshold(),
            violation_window_secs: default_window(),
            debounce_ms: default_debounce(),
            restore_source: default_restore_source(),
            escalate_after_restores: default_escalate(),
            log_each_violation: true,
            cache_budget_bytes: default_cache_budget(),
            approval: ApprovalMode::default(),
        }
    }
}

fn default_threshold() -> u32 {
    5
}
fn default_window() -> u64 {
    300
}
fn default_debounce() -> u64 {
    100
}
fn default_restore_source() -> String {
    "memory_then_store".to_string()
}
fn default_escalate() -> u32 {
    3
}
fn default_true() -> bool {
    true
}
fn default_cache_budget() -> u64 {
    256 * 1024 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespacePolicy {
    pub path: String,
    pub conflict: Option<ConflictStrategy>,
    #[serde(default)]
    pub tags_required: Vec<String>,
    pub retention: Option<RetentionPolicy>,
    /// Overrides `[global.integrity] approval` for this namespace and below.
    pub approval: Option<ApprovalMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub keep_versions: Option<u32>,
    pub keep_days: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStrategy {
    Reject,
    Version,
    Replace,
}

impl Policy {
    /// Parse from a TOML string and validate.
    pub fn from_toml(s: &str) -> Result<Self, UltnasCoreError> {
        let policy: Self = toml::from_str(s)?;
        policy.validate()?;
        Ok(policy)
    }

    /// Validate structural and semantic correctness.
    pub fn validate(&self) -> Result<(), UltnasCoreError> {
        if self.version != 1 {
            return Err(UltnasCoreError::InvalidPolicy(format!(
                "unsupported policy version: {}",
                self.version
            )));
        }
        for ns in &self.namespaces {
            NamespacePath::parse(&ns.path)?;
        }
        let valid_sources = ["memory", "store", "memory_then_store", "remote"];
        if !valid_sources.contains(&self.global.integrity.restore_source.as_str()) {
            return Err(UltnasCoreError::InvalidPolicy(format!(
                "invalid restore_source `{}` — must be one of: {}",
                self.global.integrity.restore_source,
                valid_sources.join(", ")
            )));
        }
        Ok(())
    }

    /// Approval mode for tracked files in `namespace`: the most specific
    /// namespace entry that sets one, else the global setting.
    pub fn approval_for(&self, namespace: &NamespacePath) -> ApprovalMode {
        let ns = namespace.as_str();
        self.namespaces
            .iter()
            .filter(|np| np.approval.is_some())
            .filter(|np| {
                np.path.is_empty() || ns == np.path || ns.starts_with(&format!("{}/", np.path))
            })
            .max_by_key(|np| np.path.len())
            .and_then(|np| np.approval)
            .unwrap_or(self.global.integrity.approval)
    }

    /// BLAKE3 hash of the canonical TOML serialization.
    pub fn content_id(&self) -> ContentId {
        let s = toml::to_string(self).unwrap_or_default();
        hash_bytes(s.as_bytes())
    }
}

/// Evaluates policy rules against records at ingest and rotation time.
pub struct PolicyEvaluator {
    policy: Policy,
}

impl PolicyEvaluator {
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }

    /// Evaluate an incoming record against ingest rules.
    pub fn evaluate_ingest(&self, record: &Record) -> Result<(), UltnasCoreError> {
        let ns_path = record.namespace.as_str();

        if let Some(max) = self.policy.global.max_record_size_bytes {
            if record.size_bytes > max {
                return Err(UltnasCoreError::PolicyViolation {
                    rule: "max_record_size_bytes".to_string(),
                    detail: format!("{} > {}", record.size_bytes, max),
                });
            }
        }

        let ns_policy = self
            .policy
            .namespaces
            .iter()
            .filter(|np| ns_path.starts_with(&np.path))
            .max_by_key(|np| np.path.len());

        if let Some(np) = ns_policy {
            for tag in &np.tags_required {
                if !record.tags.contains(tag) {
                    return Err(UltnasCoreError::PolicyViolation {
                        rule: "tags_required".to_string(),
                        detail: format!("missing required tag `{tag}`"),
                    });
                }
            }
        }
        Ok(())
    }

    /// Determine which records should be purged per retention policy.
    pub fn evaluate_retention(
        &self,
        namespace: &NamespacePath,
        candidates: &[Record],
    ) -> Vec<ContentId> {
        let ns_path = namespace.as_str();
        let retention = self
            .policy
            .namespaces
            .iter()
            .filter(|np| ns_path.starts_with(&np.path))
            .max_by_key(|np| np.path.len())
            .and_then(|np| np.retention.as_ref());

        let Some(ret) = retention else {
            return vec![];
        };
        let now = chrono::Utc::now();
        let mut to_purge = vec![];

        if let Some(keep_days) = ret.keep_days {
            let cutoff = now - chrono::Duration::days(keep_days as i64);
            for r in candidates {
                if r.created_at < cutoff {
                    to_purge.push(r.id);
                }
            }
        }
        to_purge
    }

    /// Convenience accessor for the integrity sub-policy.
    pub fn integrity(&self) -> &IntegrityPolicy {
        &self.policy.global.integrity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "version = 1\n[global]\nconflict = \"version\"\n";

    #[test]
    fn parse_minimal() {
        let p = Policy::from_toml(MINIMAL).unwrap();
        assert_eq!(p.version, 1);
        assert_eq!(p.global.integrity.write_violation_threshold, 5);
        assert_eq!(p.global.integrity.escalate_after_restores, 3);
    }

    #[test]
    fn integrity_defaults_populated() {
        let p = Policy::from_toml(MINIMAL).unwrap();
        let ip = &p.global.integrity;
        assert_eq!(ip.debounce_ms, 100);
        assert_eq!(ip.restore_source, "memory_then_store");
        assert!(ip.log_each_violation);
    }

    #[test]
    fn invalid_restore_source_rejected() {
        let bad = "version = 1\n[global.integrity]\nrestore_source = \"banana\"\n";
        assert!(Policy::from_toml(bad).is_err());
    }

    #[test]
    fn approval_defaults_to_automatic_and_resolves_per_namespace() {
        let ns = |s| NamespacePath::parse(s).unwrap();
        assert_eq!(
            Policy::from_toml(MINIMAL).unwrap().approval_for(&ns("a")),
            ApprovalMode::Automatic
        );

        let p = Policy::from_toml(
            r#"
            version = 1
            [global.integrity]
            approval = "approved"
            [[namespaces]]
            path = "notes"
            approval = "automatic"
            [[namespaces]]
            path = "notes/legal"
            approval = "approved"
            [[namespaces]]
            path = "notes/other"
            "#,
        )
        .unwrap();
        assert_eq!(p.approval_for(&ns("code")), ApprovalMode::Approved);
        assert_eq!(p.approval_for(&ns("notes")), ApprovalMode::Automatic);
        assert_eq!(p.approval_for(&ns("notes/other")), ApprovalMode::Automatic);
        assert_eq!(
            p.approval_for(&ns("notes/legal/nda")),
            ApprovalMode::Approved
        );
        // Segment-aware: "notesX" is not under "notes".
        assert_eq!(p.approval_for(&ns("notesX")), ApprovalMode::Approved);
    }

    #[test]
    fn invalid_approval_rejected() {
        let bad = "version = 1\n[global.integrity]\napproval = \"sometimes\"\n";
        assert!(Policy::from_toml(bad).is_err());
    }

    #[test]
    fn invalid_version_rejected() {
        assert!(Policy::from_toml("version = 99\n").is_err());
    }
}
