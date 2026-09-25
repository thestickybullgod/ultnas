use anyhow::Result;
use clap::Subcommand;
use std::path::Path;
use ultnas_core::Policy;

#[derive(Subcommand)]
pub enum PolicyCommands {
    /// Validate a policy TOML file
    Validate {
        /// Path to the policy file
        file: std::path::PathBuf,
    },
}

pub fn run(_vault_root: &Path, cmd: PolicyCommands) -> Result<()> {
    match cmd {
        PolicyCommands::Validate { file } => {
            let raw = std::fs::read_to_string(&file)?;
            Policy::from_toml(&raw)?;
            println!("✓ Policy file is valid");
        }
    }
    Ok(())
}
