//! Ultnas CLI — choose what to protect, and manage the daemon that protects it.

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use ultnas_core::{UltnasCoreError, Vault};

mod commands;
use commands::{add, daemon, init, inspect, integrity, ls, policy, purge, setup, track, verify};

#[derive(Parser)]
#[command(
    name = "ultnas",
    about = "Protect text files from invisible-character tampering",
    version,
    propagate_version = true,
    arg_required_else_help = true
)]
struct Cli {
    /// Vault to use [default: $ULTNAS_VAULT, else ~/.local/share/ultnas,
    /// or /var/lib/ultnas as root]
    #[arg(long, env = "ULTNAS_VAULT", global = true)]
    vault: Option<std::path::PathBuf>,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new vault
    Init(init::InitArgs),
    /// Archive a file or directory
    Add(add::AddArgs),
    /// Inspect a record's metadata and seal status
    Inspect(inspect::InspectArgs),
    /// Verify content integrity
    Verify(verify::VerifyArgs),
    /// List records in a namespace
    Ls(ls::LsArgs),
    /// Remove records per policy or explicit ID
    Purge(purge::PurgeArgs),
    /// Choose recommended files and directories to protect
    Setup(setup::SetupArgs),
    /// Protect a live text file (or, with --recursive, a directory) against invisible-character writes
    Track(track::TrackArgs),
    /// Stop protecting a file or directory (versions stay in the vault)
    Untrack(track::FileArg),
    /// Promote a tracked file's pending edit to its stable version
    Approve(track::FileArg),
    /// List tracked files and their status
    Tracked,
    /// Policy operations
    #[command(subcommand)]
    Policy(policy::PolicyCommands),
    /// IntegrityGuard: status, violations, quarantine management
    #[command(subcommand)]
    Integrity(integrity::IntegrityCommands),
    /// Daemon management
    #[command(subcommand)]
    Daemon(daemon::DaemonCommands),
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(log_level))
        .init();

    let vault = cli.vault.unwrap_or_else(Vault::default_root);
    let result = match cli.command {
        Commands::Init(args) => init::run(&vault, args),
        Commands::Add(args) => add::run(&vault, args),
        Commands::Inspect(args) => inspect::run(&vault, args),
        Commands::Verify(args) => verify::run(&vault, args),
        Commands::Ls(args) => ls::run(&vault, args),
        Commands::Purge(args) => purge::run(&vault, args),
        Commands::Setup(args) => setup::run(&vault, args),
        Commands::Track(args) => track::track(&vault, args),
        Commands::Untrack(args) => track::untrack(&vault, args),
        Commands::Approve(args) => track::approve(&vault, args),
        Commands::Tracked => track::list(&vault),
        Commands::Policy(cmd) => policy::run(&vault, cmd),
        Commands::Integrity(cmd) => integrity::run(&vault, cmd),
        Commands::Daemon(cmd) => daemon::run(&vault, cmd),
    };
    // Say how to get a vault, not just that there isn't one.
    result.map_err(|e| match e.downcast_ref::<UltnasCoreError>() {
        Some(UltnasCoreError::VaultNotFound(path)) => anyhow::anyhow!(
            "no vault at {} — run `ultnas setup` to create one and choose what to \
             protect (or `ultnas init --name <name>`; `--vault`/$ULTNAS_VAULT for \
             another location)",
            path.display()
        ),
        _ => e,
    })
}
