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
