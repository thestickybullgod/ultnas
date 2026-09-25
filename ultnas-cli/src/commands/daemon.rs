//! `ultnas daemon status | stop` — talk to the running daemon over IPC.

use anyhow::{bail, Context, Result};
use chrono::{Local, Utc};
use clap::Subcommand;
use std::{path::Path, time::Duration};
use ultnas_core::{
    canonical_path,
    ipc::{self, DaemonStatus, Response},
};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Subcommand)]
pub enum DaemonCommands {
    /// Show the running daemon's live state
    Status {
        /// Print the raw JSON answer
        #[arg(long)]
        json: bool,
    },
    /// Stop the daemon (it releases the vault lock and exits)
    Stop,
}

pub fn run(vault_root: &Path, cmd: DaemonCommands) -> Result<()> {
    // The endpoint is derived from the canonical path, as the daemon does.
    let root = canonical_path(vault_root)?;
    match cmd {
        DaemonCommands::Status { json } => {
            let resp = ask(&root, "status")?;
            let data = resp.data.context("status answer had no data")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&data)?);
                return Ok(());
            }
            let status: DaemonStatus =
                serde_json::from_value(data).context("unreadable status from the daemon")?;
            print_status(&status);
        }
        DaemonCommands::Stop => {
            ask(&root, "stop")?;
            println!("✓ Daemon stopping");
        }
    }
    Ok(())
}

fn ask(root: &Path, command: &str) -> Result<Response> {
    let resp = ipc::request(root, command, TIMEOUT)?;
    if !resp.ok {
        let e = resp.error.unwrap_or(ipc::IpcError {
            code: "UNKNOWN".into(),
            message: "no detail".into(),
        });
        bail!("daemon refused `{command}`: {} ({})", e.message, e.code);
    }
    Ok(resp)
}

fn print_status(s: &DaemonStatus) {
    let uptime = (Utc::now() - s.started_at).num_seconds().max(0);
    println!("ultnasd {} — PID {}", s.version, s.pid);
    println!("  vault      {}", s.vault.display());
    println!(
        "  up         {}h {:02}m {:02}s (since {})",
        uptime / 3600,
        uptime / 60 % 60,
        uptime % 60,
        s.started_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
    );

    let j = &s.journal;
    if j.degraded {
        println!(
            "  journal    DEGRADED — {} entr(y/ies) buffered, repairs suspended{}",
            j.buffered_entries,
            if j.lost_entries > 0 {
                format!(", {} lost", j.lost_entries)
            } else {
                String::new()
            }
        );
    } else {
        println!("  journal    ok");
    }
    if s.dropped_alerts > 0 {
        println!(
            "  alerts     {} dropped (the journal has the full record)",
            s.dropped_alerts
        );
    }
    if s.quarantined.is_empty() {
        println!("  quarantine none");
    } else {
        println!("  quarantine {}", s.quarantined.join(", "));
    }

    let w = &s.watcher;
    println!(
        "  watcher    {} — {} director(y/ies) watched{}",
        w.mode,
        w.watched_dirs,
        if w.unwatchable_dirs > 0 {
            format!(", {} unwatchable", w.unwatchable_dirs)
        } else {
            String::new()
        }
    );
    println!(
        "  tracking   {} file(s), {} director(y/ies)",
        w.tracked_files, w.tracked_dirs
    );
    match w.last_full_scan {
        Some(t) => println!(
            "  last scan  {}",
            t.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S")
        ),
        None => println!("  last scan  not yet"),
    }
    let c = &s.cache;
    println!(
        "  cache      {} record(s), {} of {} MiB",
        c.entries,
        c.used_bytes >> 20,
        c.budget_bytes >> 20
    );

    if !s.recent_alerts.is_empty() {
        println!("\nRecent alerts (newest last):");
        for a in &s.recent_alerts {
            println!(
                "  {}  {:<16} {}",
                a.at.with_timezone(&Local).format("%H:%M:%S"),
                a.kind,
                a.detail
            );
        }
    }
}
