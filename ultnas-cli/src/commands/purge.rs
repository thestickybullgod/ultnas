//! `ultnas purge` — remove records, by id or by the policy's retention rules.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use ultnas_core::{
    retention::{self, Purge},
    ContentId, Journal, Mirror, Policy, Vault,
};

use super::prompt::confirm;

#[derive(Args)]
pub struct PurgeArgs {
    /// ContentId of a record to purge
    pub id: Option<String>,
    /// Apply the policy's retention rules (keep_days, keep_versions)
    #[arg(long, conflicts_with = "id")]
    pub rotate: bool,
    /// Show what would be purged, and purge nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Policy file (default: the vault manifest's policy_path)
    #[arg(long)]
    pub policy: Option<PathBuf>,
    /// Don't ask for confirmation
    #[arg(long, short = 'y')]
    pub yes: bool,
}

pub fn run(vault_root: &Path, args: PurgeArgs) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let journal = Journal::open_shared(&vault.root().join("journal.log"));
    let policy = match vault.load_policy(args.policy.as_deref())? {
        Some((policy, _)) => policy,
        None => Policy::default(),
    };
    let mirror = match policy.mirror_path(vault.root()) {
        Some(p) => {
            Some(Mirror::open(&p).with_context(|| format!("opening mirror {}", p.display()))?)
        }
        None => None,
    };

    if args.rotate {
        return rotate(&vault, &policy, &journal, mirror.as_ref(), &args);
    }
    let Some(hex) = &args.id else {
        bail!("give a record id to purge, or --rotate to apply retention");
    };
    let id = ContentId::from_hex(hex)?;
    purge_one(&vault, &journal, mirror.as_ref(), id, &args)
}

fn rotate(
    vault: &Vault,
    policy: &Policy,
    journal: &Journal,
    mirror: Option<&Mirror>,
    args: &PurgeArgs,
) -> Result<()> {
    let plan = retention::rotate(vault, policy, journal, mirror, true)?;
    if plan.is_empty() {
        println!("Nothing to purge: every record is within the retention rules.");
        return Ok(());
    }
    list(&plan);
    if args.dry_run {
        println!("(dry run: nothing purged)");
        return Ok(());
    }
    if !confirm(&format!("Purge {} record(s)?", plan.len()), args.yes)? {
        println!("Nothing purged.");
        return Ok(());
    }
    // Re-planned under the lock, so it may differ slightly from the preview.
    let done = retention::rotate(vault, policy, journal, mirror, false)?;
    println!("✓ Purged {} record(s)", done.len());
    Ok(())
}

fn purge_one(
    vault: &Vault,
    journal: &Journal,
    mirror: Option<&Mirror>,
    id: ContentId,
    args: &PurgeArgs,
) -> Result<()> {
    let record = vault.get_record(&id)?;
    let p = Purge {
        id,
        namespace: record.namespace.as_str(),
        label: record.label.clone(),
        reason: "explicit".into(),
    };
    let users: Vec<PathBuf> = vault
        .tracked_files()?
        .into_iter()
        .filter(|t| t.stable == id || t.pending == Some(id))
        .map(|t| t.path)
        .collect();
    if let Some(first) = users.first() {
        bail!(
            "{} is a current version of {} — untrack it first",
            id,
            first.display()
        );
    }
    list(std::slice::from_ref(&p));
    if record.is_sealed() {
        println!("  This record is sealed.");
    }
    if args.dry_run {
        println!("(dry run: nothing purged)");
        return Ok(());
    }
    if !confirm("Purge it?", args.yes)? {
        println!("Nothing purged.");
        return Ok(());
    }
    vault.with_tracking_lock(|| {
        // Re-check under the lock: it may have become a current version.
        if vault
            .tracked_files()?
            .iter()
            .any(|t| t.stable == id || t.pending == Some(id))
        {
            return Err(ultnas_core::UltnasCoreError::InvalidPolicy(
                "it just became a tracked file's current version; not purged".into(),
            ));
        }
        journal.write(retention::purge_entry(&p, "operator"))?;
        vault.purge_record(&id)?;
        if let Some(m) = mirror {
            m.remove(&id)?;
        }
        Ok(())
    })?;
    println!("✓ Purged {id}");
    Ok(())
}

fn list(purges: &[Purge]) {
    for p in purges {
        let ns = if p.namespace.is_empty() {
            "(root)"
        } else {
            &p.namespace
        };
        println!(
            "  {}  {:<16} {:<24} {}",
            &p.id.to_hex()[..12],
            ns,
            p.label,
            p.reason
        );
    }
}
