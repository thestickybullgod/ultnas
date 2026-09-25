use anyhow::Result;
use clap::Args;
use std::path::Path;

#[derive(Args)]
pub struct PurgeArgs {
    /// ContentId to purge
    pub id: Option<String>,
    /// Apply retention policy to the namespace
    #[arg(long)]
    pub rotate: bool,
}

pub fn run(_vault_root: &Path, _args: PurgeArgs) -> Result<()> {
    // TODO(v0.4): Implement purge and rotate
    eprintln!("purge: not yet implemented (planned for v0.4)");
    Ok(())
}
