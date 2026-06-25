// SPDX-License-Identifier: Apache-2.0

//! Cross-cutting helpers shared by the `cmd/*` modules.

use eyre::{Context, Result};

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
        eyre::bail!("vectors reset drops the existing vectors; pass --yes to confirm");
    }
    println!("This drops the chunks table and re-embeds every book from the corpus tree.");
    println!("The old vectors are unrecoverable.");
    confirm_token("Type RESET (exact, uppercase) to continue: ", "RESET")
}

/// Recursively collect every `.pdf` file under `dir`. The directory is
/// walked breadth-first; unreadable subdirectories are skipped. Used by
/// the paper-side ingest dispatch to expand `--recursive` into the
/// `paths` array `glean.submit` expects.
pub fn collect_pdf_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn is_pdf(p: &std::path::Path) -> bool {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false)
    }
    fn visit(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                visit(&p, out);
            } else if is_pdf(&p) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    visit(dir, &mut out);
    out
}
