// SPDX-License-Identifier: Apache-2.0

//! Uniform destructive-action confirmation primitives.
//!
//! Two strengths: [`ConfirmMode::Soft`] accepts a case-insensitive
//! `yes` or `y`; [`ConfirmMode::Hard`] requires the operator to retype
//! a literal token (typically the object's name or an upper-case
//! sentinel such as `RESET`). The wrapper also honours an
//! `assume_yes` short-circuit that callers use to thread `--yes`
//! flags through without duplicating the read-stdin path.

use std::io::{self, BufRead, Write};

/// Confirmation strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmMode<'a> {
    /// Accept `yes` or `y` (case-insensitive).
    Soft,
    /// Require the operator to retype the given token verbatim.
    Hard { token: &'a str },
}

/// Reads one line from stdin and decides whether the operator agreed.
/// Returns `Ok(false)` on an empty line, EOF, or a mismatched token.
pub fn confirm_destructive(
    prompt: &str,
    mode: ConfirmMode<'_>,
    assume_yes: bool,
) -> io::Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    let stderr = io::stderr();
    let mut stderr_lock = stderr.lock();
    write!(stderr_lock, "{prompt} ")?;
    stderr_lock.flush()?;
    drop(stderr_lock);

    let stdin = io::stdin();
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line)?;
    if read == 0 {
        return Ok(false);
    }
    let entered = line.trim();
    Ok(match mode {
        ConfirmMode::Soft => matches!(entered.to_ascii_lowercase().as_str(), "yes" | "y"),
        ConfirmMode::Hard { token } => entered == token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assume_yes_short_circuits_both_modes() {
        assert!(confirm_destructive("p", ConfirmMode::Soft, true).unwrap());
        assert!(confirm_destructive("p", ConfirmMode::Hard { token: "RESET" }, true).unwrap());
    }
}
