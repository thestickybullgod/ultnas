use anyhow::Result;
use clap::Args;
use std::path::Path;
use ultnas_core::{ContentId, Vault};

#[derive(Args)]
pub struct VerifyArgs {
    /// Verify a specific record (by ContentId)
    pub id: Option<String>,
    /// Verify all records in the vault
    #[arg(long)]
    pub all: bool,
}

pub fn run(vault_root: &Path, args: VerifyArgs) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    if args.all {
        let verified = vault.verify_all()?;
        println!("✓ All {} record(s) verified OK", verified.len());
    } else if let Some(id_str) = args.id {
        let id = ContentId::from_hex(&id_str)?;
        vault.verify(&id)?;
        println!("✓ Record {} verified OK", id_str);
    } else {
        eprintln!("Provide a record ID or --all");
    }
    Ok(())
}
