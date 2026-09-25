use anyhow::Result;
use clap::Args;
use std::path::Path;
use ultnas_core::{NamespacePath, RecordBuilder, Vault};

#[derive(Args)]
pub struct AddArgs {
    /// File to archive
    pub file: std::path::PathBuf,
    /// Namespace to archive into
    #[arg(long, default_value = "")]
    pub namespace: String,
    /// Label for the record (defaults to filename)
    #[arg(long)]
    pub label: Option<String>,
    /// Tags (repeatable)
    #[arg(long = "tag", short = 't')]
    pub tags: Vec<String>,
}

pub fn run(vault_root: &Path, args: AddArgs) -> Result<()> {
    let vault = Vault::open(vault_root)?;
    let content = std::fs::read(&args.file)?;
    let label = args.label.unwrap_or_else(|| {
        args.file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    let ns = NamespacePath::parse(&args.namespace)?;
    let mime = mime_guess_from_path(&args.file);
    let mut builder = RecordBuilder::new(ns, label).media_type(mime);
    for tag in &args.tags {
        builder = builder.tag(tag);
    }
    let record = builder.build(&content)?;
    let id = record.id;
    vault.write_record(&record, &content)?;
    println!("✓ Archived  {}", id.to_hex());
    Ok(())
}

fn mime_guess_from_path(path: &std::path::Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("md") => "text/markdown",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
    .to_string()
}
