// SPDX-License-Identifier: Apache-2.0

//! Cross-cutting helpers shared by the `cmd/*` modules: query
//! input shaping, resolution-source label rendering, and a
//! synchronous stdin confirmation prompt.

use anyhow::{Context, Result};

/// Hard cap on the query text the embedder is asked to vectorize. The
/// embedding model has its own context window; sending tens of
/// kilobytes of text yields a low-quality vector and silently masks
/// the operator's intent. The cap is generous — long-form passages
/// commonly fit under 4 KiB — but bounded so a paste of an entire
/// document is recognized as user error rather than rolling forward
/// with a noisy hit set.
pub const MAX_QUERY_BYTES: usize = 4096;

/// Truncate a `query` string at [`MAX_QUERY_BYTES`] and warn on stderr
/// when truncation happened. Returns the truncated text as an owned
/// `String`; short inputs are echoed verbatim so callers can borrow
/// it without conditional handling. The cut respects a UTF-8 char
/// boundary so the embedder never sees a half-encoded glyph.
pub fn truncate_query_with_warning(query: &str) -> String {
    if query.len() <= MAX_QUERY_BYTES {
        return query.to_string();
    }
    let mut boundary = MAX_QUERY_BYTES;
    while boundary > 0 && !query.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let truncated = &query[..boundary];
    eprintln!(
        "bookrack: query was {} bytes, longer than the {} byte limit; truncated before embedding",
        query.len(),
        MAX_QUERY_BYTES
    );
    truncated.to_string()
}

/// Resolve the runtime `resolution_source` string back to the
/// `&'static str` the InfoSnapshot carries. Matches the labels
/// produced by [`resolution_source_label`].
pub fn static_source_label(source: &str) -> &'static str {
    match source {
        "--data-dir flag" => "--data-dir flag",
        "--library flag" => "--library flag",
        "BOOKRACK_DATA_DIR env" => "BOOKRACK_DATA_DIR env",
        "portable layout" => "portable layout",
        "registry default" => "registry default",
        "default registry default" => "default registry default",
        "explicit" => "explicit",
        _ => "(unknown)",
    }
}

/// Strongly-typed sibling of [`static_source_label`]. Used by the
/// daemon-REPL session header where the source is held as the typed
/// enum rather than a serialised string.
pub fn resolution_source_label(source: bookrack_config::ResolutionSource) -> &'static str {
    use bookrack_config::ResolutionSource::*;
    match source {
        DataDirFlag => "--data-dir flag",
        LibraryFlag => "--library flag",
        EnvVar => "BOOKRACK_DATA_DIR env",
        PortableExeNeighbor => "portable layout",
        RegistryDefault => "registry default",
        DefaultRegistryDefault => "default registry default",
        Explicit => "explicit",
    }
}

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
