//! Ultnas desktop app: see what the daemon is doing, choose what to protect,
//! approve pending edits, and get notified when Ultnas steps in.
//!
//! Linux only (GTK 4 + libadwaita). Everything the app changes goes through
//! `ultnas_core::ops`, the same code the CLI uses.

// Off Linux only the stub `main` is built, so the shared modules go unused.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod diff;
mod events;
mod settings;

#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod pages;
#[cfg(target_os = "linux")]
mod ui;

use std::path::PathBuf;

/// Application id: D-Bus name, desktop file, icon, and notifications.
pub const APP_ID: &str = "org.planetarchives.Ultnas";

const USAGE: &str = "\
Usage: ultnas-gui [--vault <PATH>] [--background]

Options:
      --vault <PATH>  Vault to use [default: $ULTNAS_VAULT, else
                      ~/.local/share/ultnas, or /var/lib/ultnas as root]
      --background    Start without a window (notifications only)
  -h, --help          Print help
  -V, --version       Print version";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub vault: Option<PathBuf>,
    pub background: bool,
}

enum Parsed {
    Run(Args),
    /// Print this and exit successfully.
    Info(String),
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut out = Args::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Info(USAGE.to_string())),
            "-V" | "--version" => {
                return Ok(Parsed::Info(format!(
                    "ultnas-gui {}",
                    env!("CARGO_PKG_VERSION")
                )))
            }
            "--background" => out.background = true,
            "--vault" => {
                let v = args.next().ok_or("--vault needs a path")?;
                out.vault = Some(PathBuf::from(v));
            }
            s if s.starts_with("--vault=") => {
                out.vault = Some(PathBuf::from(&s["--vault=".len()..]));
            }
            s => return Err(format!("unexpected argument `{s}`\n\n{USAGE}")),
        }
    }
    if out.vault.is_none() {
        out.vault = std::env::var_os("ULTNAS_VAULT")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
    }
    Ok(Parsed::Run(out))
}

fn main() -> std::process::ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Parsed::Run(args)) => args,
        Ok(Parsed::Info(text)) => {
            println!("{text}");
            return std::process::ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    run(args)
}

#[cfg(target_os = "linux")]
fn run(args: Args) -> std::process::ExitCode {
    app::run(args)
}

#[cfg(not(target_os = "linux"))]
fn run(_args: Args) -> std::process::ExitCode {
    eprintln!("The Ultnas desktop app runs on Linux only; use the `ultnas` CLI here.");
    std::process::ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Parsed, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn flags_parse() {
        let Ok(Parsed::Run(a)) = parse(&["--vault", "/v", "--background"]) else {
            panic!("expected Run");
        };
        assert_eq!(a.vault.as_deref(), Some(std::path::Path::new("/v")));
        assert!(a.background);
        let Ok(Parsed::Run(a)) = parse(&["--vault=/w"]) else {
            panic!("expected Run");
        };
        assert_eq!(a.vault.as_deref(), Some(std::path::Path::new("/w")));
    }

    #[test]
    fn version_and_help_print_and_bad_args_fail() {
        assert!(
            matches!(parse(&["--version"]), Ok(Parsed::Info(v)) if v.starts_with("ultnas-gui "))
        );
        assert!(matches!(parse(&["-h"]), Ok(Parsed::Info(u)) if u.starts_with("Usage")));
        assert!(parse(&["--vault"]).is_err());
        assert!(parse(&["--frobnicate"]).is_err());
    }
}
