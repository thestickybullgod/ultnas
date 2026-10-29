//! `ultnas integrity` — inspect and manage IntegrityGuard state.
//!
//! Subcommands:
//!   status            — show violation counts and quarantined namespaces
//!   lift-quarantine   — remove a namespace from quarantine (operator action)
//!   violations        — list recent write violations from the journal

use anyhow::Result;
use clap::Subcommand;
use std::path::Path;
use ultnas_core::{
    ops::{self, Actor},
    Journal, JournalOp,
};

#[derive(Subcommand)]
pub enum IntegrityCommands {
    /// Show IntegrityGuard status: quarantined namespaces and restore counts
    Status,
    /// List recent write violations from the journal
    Violations {
        /// Maximum number of entries to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Lift a namespace quarantine (requires operator confirmation)
    LiftQuarantine {
        /// Namespace path to lift quarantine from ("(root)" for the root namespace)
        namespace: String,
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Show the count of integrity restores performed
    RestoreCount,
}

pub fn run(vault_root: &Path, cmd: IntegrityCommands) -> Result<()> {
    match cmd {
        IntegrityCommands::Status => cmd_status(vault_root),
        IntegrityCommands::Violations { limit } => cmd_violations(vault_root, limit),
        IntegrityCommands::LiftQuarantine { namespace, yes } => {
            cmd_lift_quarantine(vault_root, &namespace, yes)
        }
        IntegrityCommands::RestoreCount => cmd_restore_count(vault_root),
    }
}

fn cmd_status(vault_root: &Path) -> Result<()> {
    let journal_path = vault_root.join("journal.log");

    if !journal_path.exists() {
        println!("No journal found — daemon has not run yet.");
        return Ok(());
    }

    let journal = Journal::open_shared(&journal_path);

    let violations = journal.count_op(&JournalOp::WriteViolation)?;
    let restores = journal.count_op(&JournalOp::IntegrityRestore)?;
    let failed = journal.count_op(&JournalOp::IntegrityRestoreFailed)?;
    let escalations = journal.count_op(&JournalOp::IntegrityEscalate)?;
    let lifts = journal.count_op(&JournalOp::QuarantineLift)?;

    // Same fold the daemon uses, so the two can never disagree.
    let quarantined = journal.quarantined_namespaces()?;
    let active_quarantines = quarantined.len();

    println!("╔══════════════════════════════════════════╗");
    println!("║       Ultnas IntegrityGuard Status        ║");
    println!("╠══════════════════════════════════════════╣");
    println!("║  Write violations detected : {:>10}  ║", violations);
    println!("║  Integrity restores fired  : {:>10}  ║", restores);
    println!("║  Integrity restores failed : {:>10}  ║", failed);
    println!("║  Namespace escalations     : {:>10}  ║", escalations);
    println!("║  Quarantine lifts          : {:>10}  ║", lifts);
    println!(
        "║  Active quarantines        : {:>10}  ║",
        active_quarantines
    );
    println!("╚══════════════════════════════════════════╝");
    let archives = journal.archives();
    if !archives.is_empty() {
        println!(
            "  Counts cover the current journal only; {} older rotated file(s) \
             ({}.1 …) are not included. Quarantines are carried over.",
            archives.len(),
            journal_path.display()
        );
    }

    if active_quarantines > 0 {
        println!(
            "\n⚠  {} namespace(s) are currently quarantined:",
            active_quarantines
        );
        for ns in &quarantined {
            println!("     {}", ops::namespace_label(ns));
        }
        println!("   Run `ultnas integrity violations` to inspect the journal.");
        println!("   Run `ultnas integrity lift-quarantine <namespace>` to unquarantine.");
    } else {
        println!("\n✓  No active quarantines.");
    }

    Ok(())
}

fn cmd_violations(vault_root: &Path, limit: usize) -> Result<()> {
    let journal_path = vault_root.join("journal.log");
    if !journal_path.exists() {
        println!("No journal found.");
        return Ok(());
    }

    let journal = Journal::open_shared(&journal_path);
    let relevant_ops = [
        JournalOp::WriteViolation,
        JournalOp::IntegrityRestoreIntent,
        JournalOp::IntegrityRestore,
        JournalOp::IntegrityRestoreFailed,
        JournalOp::IntegrityEscalate,
        JournalOp::QuarantineLift,
    ];

    let entries: Vec<_> = journal
        .iter()?
        .filter_map(Result::ok)
        .filter(|e| relevant_ops.contains(&e.op))
        .collect();

    let total = entries.len();
    let show: Vec<_> = entries.into_iter().rev().take(limit).collect();

    if show.is_empty() {
        println!("✓ No integrity events in journal.");
        return Ok(());
    }

    println!(
        "{:<28} {:<22} {:<20} DETAIL",
        "TIMESTAMP", "OP", "ID (first 16)"
    );
    println!("{}", "─".repeat(90));
    for e in show.iter().rev() {
        let ts = e.ts.format("%Y-%m-%d %H:%M:%S UTC");
        let op = format!("{:?}", e.op);
        let id =
            e.id.map(|i| i.to_hex()[..16].to_string())
                .unwrap_or_else(|| "—".into());
        let ns = e.ns.as_deref().unwrap_or("—");
        let detail = e.detail.as_deref().unwrap_or(ns);
        println!("{:<28} {:<22} {:<20} {}", ts, op, id, detail);
    }

    if total > limit {
        println!(
            "\n  … {} earlier events not shown (increase --limit to see more)",
            total - limit
        );
    }

    Ok(())
}

fn cmd_lift_quarantine(vault_root: &Path, namespace: &str, yes: bool) -> Result<()> {
    let ns = ops::parse_namespace(namespace)?;
    let namespace = ns.as_str();
    let label = ops::namespace_label(&namespace);
    let Some(quarantined) = ops::quarantined(vault_root)? else {
        println!("No journal found — nothing is quarantined.");
        return Ok(());
    };
    if !quarantined.contains(&namespace) {
        println!(
            "Namespace `{}` is not quarantined — nothing to lift.",
            label
        );
        if !quarantined.is_empty() {
            let list: Vec<&str> = quarantined
                .iter()
                .map(|n| ops::namespace_label(n))
                .collect();
            println!("Currently quarantined: {}", list.join(", "));
        }
        return Ok(());
    }

    if !yes {
        println!("This will lift the quarantine on namespace `{}`.", label);
        println!("The IntegrityGuard will resume auto-restoring it, with violation counts reset.");
        print!("Confirm? [y/N] ");
        use std::io::{self, BufRead, Write};
        io::stdout().flush()?;
        let stdin = io::stdin();
        let line = stdin.lock().lines().next().unwrap_or(Ok(String::new()))?;
        if !line.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    ops::lift_quarantine(vault_root, &ns, Actor::Cli)?;

    println!("✓ Quarantine lift recorded for namespace `{}`.", label);
    println!("  A running daemon applies it as soon as it sees the journal change.");
    Ok(())
}

fn cmd_restore_count(vault_root: &Path) -> Result<()> {
    let journal_path = vault_root.join("journal.log");
    if !journal_path.exists() {
        println!("0 restores (no journal found).");
        return Ok(());
    }
    let journal = Journal::open_shared(&journal_path);
    let count = journal.count_op(&JournalOp::IntegrityRestore)?;
    println!("{} integrity restore(s) recorded in journal.", count);
    Ok(())
}
