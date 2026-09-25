//! Interactive confirmation shared by commands.

use anyhow::{bail, Result};
use std::io::{self, BufRead, IsTerminal, Write};

/// Ask a yes/no question (default no). `assume_yes` answers yes without
/// asking; without a terminal to ask on, refuse rather than guess.
pub fn confirm(question: &str, assume_yes: bool) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        bail!("{question} — confirmation needed, but stdin isn't a terminal; pass --yes");
    }
    print!("{question} [y/N] ");
    io::stdout().flush()?;
    let line = io::stdin()
        .lock()
        .lines()
        .next()
        .unwrap_or(Ok(String::new()))?;
    Ok(line.trim().eq_ignore_ascii_case("y") || line.trim().eq_ignore_ascii_case("yes"))
}
