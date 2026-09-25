use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum DaemonCommands {
    /// Show daemon status
    Status,
    /// Stop the daemon
    Stop,
}

pub fn run(cmd: DaemonCommands) -> Result<()> {
    match cmd {
        DaemonCommands::Status => {
            // TODO(v0.3): Query daemon over IPC socket
            eprintln!("daemon status: IPC not yet implemented (planned for v0.3)");
        }
        DaemonCommands::Stop => {
            eprintln!("daemon stop: not yet implemented");
        }
    }
    Ok(())
}
