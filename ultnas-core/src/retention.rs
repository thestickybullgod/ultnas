//! Retention: purging records per the policy's `[namespaces.retention]`.
//!
//! - `keep_days`: records older than this are purged.
//! - `keep_versions`: only the newest N versions of each *series* are kept.
//!   A series is one tracked file's history (its versions share a
//!   `source_path`), or, for other records, one label in the namespace.
//! - `require_seal_before_rotation`: only sealed records may be purged.
//!
//! A version a tracked file currently uses (its stable or pending version)
//! is never purged, though it counts toward the N kept. Rotation holds the
//! tracking lock throughout, so no edit can make a version current while
//! it is being deleted. Each purge is journaled before anything is removed,
//! and rotation stops at the first entry that can't be written.

use crate::{
    ContentId, Journal, JournalEntry, JournalOp, Mirror, Policy, Record, UltnasCoreError, Vault,
};
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};

/// One record rotation would purge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Purge {
    pub id: ContentId,
    pub namespace: String,
    pub label: String,
    pub reason: String,
}

/// What retention would purge now. `protected` ids are never included.
pub fn plan(
    records: &[Record],
    policy: &Policy,
    protected: &HashSet<ContentId>,
    now: DateTime<Utc>,
) -> Vec<Purge> {
    let require_seal = policy.global.require_seal_before_rotation;
    let eligible = |r: &Record| !protected.contains(&r.id) && (!require_seal || r.is_sealed());
    let purge = |r: &Record, reason: String| Purge {
        id: r.id,
        namespace: r.namespace.as_str(),
        label: r.label.clone(),
        reason,
    };

    let mut out: HashMap<ContentId, Purge> = HashMap::new();
    let mut series: BTreeMap<(String, String), Vec<&Record>> = BTreeMap::new();
    for r in records {
        let Some(ret) = policy.retention_for(&r.namespace) else {
            continue;
        };
        if let Some(days) = ret.keep_days {
            if r.created_at < now - Duration::days(days.into()) && eligible(r) {
                out.insert(r.id, purge(r, format!("older than {days} day(s)")));
            }
        }
        if ret.keep_versions.is_some() {
            let key = r
                .metadata
                .get("source_path")
                .cloned()
                .unwrap_or_else(|| format!("label:{}", r.label));
            series
                .entry((r.namespace.as_str(), key))
                .or_default()
                .push(r);
        }
    }
    for versions in series.values_mut() {
        let Some(keep) = policy
            .retention_for(&versions[0].namespace)
            .and_then(|ret| ret.keep_versions)
        else {
            continue;
        };
        versions.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        for r in versions.iter().skip(keep as usize) {
            if eligible(r) {
                out.entry(r.id)
                    .or_insert_with(|| purge(r, format!("beyond the newest {keep} version(s)")));
            }
        }
    }
    let mut out: Vec<Purge> = out.into_values().collect();
    out.sort_by_key(|p| (p.namespace.clone(), p.label.clone(), p.id.to_hex()));
    out
}

/// Apply retention to the vault. With `dry_run`, only report. Returns what
/// was (or would be) purged.
pub fn rotate(
    vault: &Vault,
    policy: &Policy,
    journal: &Journal,
    mirror: Option<&Mirror>,
    dry_run: bool,
) -> Result<Vec<Purge>, UltnasCoreError> {
    vault.with_tracking_lock(|| {
        let protected: HashSet<ContentId> = vault
            .tracked_files()?
            .iter()
            .flat_map(|t| [Some(t.stable), t.pending])
            .flatten()
            .collect();
        let purges = plan(&vault.all_records()?, policy, &protected, Utc::now());
        if dry_run {
            return Ok(purges);
        }
        let mut done = Vec::with_capacity(purges.len());
        for p in purges {
            journal.write(purge_entry(&p, "retention"))?;
            vault.purge_record(&p.id)?;
            if let Some(m) = mirror {
                m.remove(&p.id)?;
            }
            done.push(p);
        }
        Ok(done)
    })
}

/// Journal entry recording a purge (written before the purge happens).
pub fn purge_entry(p: &Purge, by: &str) -> JournalEntry {
    JournalEntry {
        ts: Utc::now(),
        op: JournalOp::PurgeRecord,
        id: Some(p.id),
        ns: Some(p.namespace.clone()),
        label: Some(p.label.clone()),
        size: None,
        detail: Some(format!("{by}: {}", p.reason)),
        path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hash_bytes, NamespacePath, RecordBuilder, TrackedFile};
    use tempfile::TempDir;

    fn policy(toml: &str) -> Policy {
        Policy::from_toml(toml).unwrap()
    }

    fn record(ns: &str, label: &str, body: &str, age_days: i64, source: Option<&str>) -> Record {
        let mut b = RecordBuilder::new(NamespacePath::parse(ns).unwrap(), label);
        if let Some(s) = source {
            b = b.metadata("source_path", s);
        }
        let mut r = b.build(body.as_bytes()).unwrap();
        r.created_at = Utc::now() - Duration::days(age_days);
        r
    }

    fn ids(p: &[Purge]) -> HashSet<ContentId> {
        p.iter().map(|p| p.id).collect()
    }

    const KEEP: &str = r#"
        version = 1
        [[namespaces]]
        path = "docs"
          [namespaces.retention]
          keep_versions = 2
          keep_days = 30
    "#;

    #[test]
    fn keep_days_and_keep_versions() {
        let (a1, a2, a3) = (
            record("docs", "a", "a1", 3, None),
            record("docs", "a", "a2", 2, None),
            record("docs", "a", "a3", 1, None),
        );
        let old = record("docs", "b", "old", 40, None);
        let recs = vec![a1.clone(), a2.clone(), a3.clone(), old.clone()];
        let got = ids(&plan(&recs, &policy(KEEP), &HashSet::new(), Utc::now()));
        // a1 is the third-newest "a"; "old" is past 30 days.
        assert_eq!(got, [a1.id, old.id].into());
    }

    #[test]
    fn series_are_per_tracked_file_not_per_label() {
        let x1 = record("docs", "mod.rs", "x1", 3, Some("/p/x/mod.rs"));
        let x2 = record("docs", "mod.rs", "x2", 2, Some("/p/x/mod.rs"));
        let y1 = record("docs", "mod.rs", "y1", 3, Some("/p/y/mod.rs"));
        let got = plan(&[x1, x2, y1], &policy(KEEP), &HashSet::new(), Utc::now());
        assert!(
            got.is_empty(),
            "two files, two versions at most each: {got:?}"
        );
    }

    #[test]
    fn protected_versions_are_kept_but_count() {
        let v: Vec<_> = (0..4)
            .map(|i| record("docs", "a", &format!("v{i}"), 10 - i, None))
            .collect();
        // The oldest is some tracked file's stable version.
        let protected: HashSet<_> = [v[0].id].into();
        let got = ids(&plan(&v, &policy(KEEP), &protected, Utc::now()));
        assert_eq!(
            got,
            [v[1].id].into(),
            "keep newest 2 (v3, v2); v0 protected"
        );
    }

    #[test]
    fn require_seal_and_unconfigured_namespaces() {
        let sealed_policy = policy(&format!(
            "{KEEP}\n[global]\nrequire_seal_before_rotation = true\n"
        ));
        let mut sealed = record("docs", "s", "sealed", 40, None);
        sealed
            .seal_record("pk".into(), "sig".into(), hash_bytes(b"p"))
            .unwrap();
        let unsealed = record("docs", "u", "unsealed", 40, None);
        let elsewhere = record("other", "o", "elsewhere", 400, None);
        let prefix_trap = record("docsX", "t", "trap", 400, None);
        let got = ids(&plan(
            &[sealed.clone(), unsealed, elsewhere, prefix_trap],
            &sealed_policy,
            &HashSet::new(),
            Utc::now(),
        ));
        assert_eq!(got, [sealed.id].into());
    }

    #[test]
    fn rotate_purges_journals_and_spares_tracked_versions() {
        let dir = TempDir::new().unwrap();
        let vault = Vault::init(&dir.path().join("v"), "t").unwrap();
        let journal = Journal::open(&vault.root().join("journal.log")).unwrap();
        let mirror = Mirror::open(&dir.path().join("m")).unwrap();

        let old = record("docs", "b", "old", 40, None);
        vault.write_record(&old, b"old").unwrap();
        mirror.store(&old.id, b"old").unwrap();
        let tracked_old = record("docs", "c", "tracked", 40, None);
        vault.write_record(&tracked_old, b"tracked").unwrap();
        let live = dir.path().join("c.txt");
        vault
            .update_tracked(&live, |t| {
                *t = Some(TrackedFile {
                    path: live.clone(),
                    namespace: NamespacePath::parse("docs").unwrap(),
                    stable: tracked_old.id,
                    pending: None,
                    updated_at: Utc::now(),
                    source: None,
                });
                Ok(())
            })
            .unwrap();

        let p = policy(KEEP);
        let preview = rotate(&vault, &p, &journal, Some(&mirror), true).unwrap();
        assert_eq!(ids(&preview), [old.id].into());
        assert!(vault.get_record(&old.id).is_ok(), "dry run changes nothing");

        let done = rotate(&vault, &p, &journal, Some(&mirror), false).unwrap();
        assert_eq!(ids(&done), [old.id].into());
        assert!(vault.get_record(&old.id).is_err());
        assert!(!vault.object_path(&old.id).exists());
        assert!(!mirror.has_valid(&old.id));
        assert!(
            vault.verify(&tracked_old.id).is_ok(),
            "tracked version kept"
        );
        assert_eq!(journal.count_op(&JournalOp::PurgeRecord).unwrap(), 1);
    }
}
