// SPDX-License-Identifier: Apache-2.0

//! Cross-cutting helpers shared by the `cmd/*` modules.

use anyhow::{Context, Result};

/// Read a confirmation token from stdin: only the literal "yes"
/// (case-insensitive, trimmed) passes.
pub fn confirm(prompt: &str) -> Result<bool> {
    use std::io::{Write, stdin, stdout};
    print!("{prompt}");
    stdout().flush().context("flush stdout")?;
    let mut buf = String::new();
    stdin().read_line(&mut buf).context("read confirmation")?;
    Ok(buf.trim().eq_ignore_ascii_case("yes"))
}

/// Read a confirmation token from stdin and accept it only when it
/// matches `expected` exactly (case-sensitive, trimmed).
///
/// Stronger than [`confirm`] — used for destructive operations where
/// "yes" reflexive-typing would be a footgun. The caller picks a
/// command-specific sentinel (e.g. `RESET`) that a user must type
/// deliberately.
pub fn confirm_token(prompt: &str, expected: &str) -> Result<bool> {
    use std::io::{Write, stdin, stdout};
    print!("{prompt}");
    stdout().flush().context("flush stdout")?;
    let mut buf = String::new();
    stdin().read_line(&mut buf).context("read confirmation")?;
    Ok(buf.trim() == expected)
}

/// Gate `vectors reset` behind the `RESET` sentinel.
///
/// Returns `Ok(true)` when the run may proceed: `--yes` was passed,
/// the invocation resumes an interrupted reset (which re-runs no
/// destructive step), or the user typed the sentinel at an
/// interactive prompt. Returns `Ok(false)` when the user declined.
/// Errors when stdin is not a terminal and `--yes` is absent, so a
/// scripted caller must opt in explicitly.
pub fn confirm_vectors_reset(yes: bool, resume: bool) -> Result<bool> {
    use std::io::IsTerminal;
    if yes || resume {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("vectors reset drops the existing vectors; pass --yes to confirm");
    }
    println!("This drops the chunks table and re-embeds every book from the corpus tree.");
    println!("The old vectors are unrecoverable.");
    confirm_token("Type RESET (exact, uppercase) to continue: ", "RESET")
}
