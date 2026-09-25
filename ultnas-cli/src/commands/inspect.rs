use anyhow::Result;
use clap::Args;
use std::path::Path;
use ultnas_core::{ContentId, Vault};

#[derive(Args)]
pub struct InspectArgs {
    /// ContentId of the record to inspect
    pub id: String,
    /// Show seal details
    #[arg(long)]
    pub show_seal: bool,
}

pub fn run(vault_root: &Path, args: InspectArgs) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let id = ContentId::from_hex(&args.id)?;
    let record = vault.get_record(&id)?;

    println!("id:          {}", record.id);
    println!("label:       {}", record.label);
    println!("namespace:   {}", record.namespace);
    println!(
        "created_at:  {}",
        record.created_at.format("%Y-%m-%d %H:%M:%S UTC")
    );
    println!("media_type:  {}", record.media_type);
    println!("size:        {} bytes", record.size_bytes);
    println!("tags:        {}", record.tags.join(", "));
    println!("sealed:      {}", record.is_sealed());
    if args.show_seal {
        if let Some(seal) = &record.seal {
            println!(
                "sealed_at:   {}",
                seal.sealed_at.format("%Y-%m-%d %H:%M:%S UTC")
            );
            println!("sealed_by:   {}", seal.sealed_by);
            println!("policy_hash: {}", seal.policy_hash);
        }
    }
    if !record.metadata.is_empty() {
        println!("metadata:");
        for (k, v) in &record.metadata {
            println!("  {k}: {v}");
        }
    }
    Ok(())
}
