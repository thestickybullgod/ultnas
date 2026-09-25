//! Ultnas CLI — sovereign archiving from the terminal.

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod commands;
use commands::{add, daemon, init, inspect, integrity, ls, policy, purge, verify};

#[derive(Parser)]
#[command(
    name = "ultnas",
    about = "Sovereign, policy-driven digital archiving",
    version,
    propagate_version = true,
    arg_required_else_help = true
)]
struct Cli {
    /// Path to the vault root (defaults to current directory)
    #[arg(short, long, default_value = ".", global = true)]
    vault: std::path::PathBuf,

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

    match cli.command {
        Commands::Init(args) => init::run(&cli.vault, args),
        Commands::Add(args) => add::run(&cli.vault, args),
        Commands::Inspect(args) => inspect::run(&cli.vault, args),
        Commands::Verify(args) => verify::run(&cli.vault, args),
        Commands::Ls(args) => ls::run(&cli.vault, args),
        Commands::Purge(args) => purge::run(&cli.vault, args),
        Commands::Policy(cmd) => policy::run(&cli.vault, cmd),
        Commands::Integrity(cmd) => integrity::run(&cli.vault, cmd),
        Commands::Daemon(cmd) => daemon::run(cmd),
    }
}
