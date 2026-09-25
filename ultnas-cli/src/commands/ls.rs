use anyhow::Result;
use clap::Args;
use std::path::Path;
use ultnas_core::{NamespacePath, Vault};

#[derive(Args)]
pub struct LsArgs {
    /// Namespace to list (defaults to root)
    #[arg(default_value = "")]
    pub namespace: String,
}

pub fn run(vault_root: &Path, args: LsArgs) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let ns = NamespacePath::parse(&args.namespace)?;
    let records = vault.list_records(&ns)?;
    if records.is_empty() {
        println!("(no records in namespace `{}`)", ns);
        return Ok(());
    }
    println!("{:<64}  {:<30}  CREATED", "ID", "LABEL");
    for r in &records {
        println!(
            "{:<64}  {:<30}  {}",
            r.id,
            r.label,
            r.created_at.format("%Y-%m-%d")
        );
    }
    Ok(())
}
