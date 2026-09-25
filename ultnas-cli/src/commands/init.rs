use anyhow::Result;
use clap::Args;
use std::path::Path;
use ultnas_core::Vault;

#[derive(Args)]
pub struct InitArgs {
    /// Name for the new vault
    #[arg(long)]
    pub name: String,
}

pub fn run(vault_root: &Path, args: InitArgs) -> Result<()> {
    let vault = Vault::init(vault_root, &args.name)?;
    println!(
        "✓ Initialized vault `{}` at {}",
        args.name,
        vault.root().display()
    );
    Ok(())
}
