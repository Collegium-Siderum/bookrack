// SPDX-License-Identifier: Apache-2.0

//! [`Options`] for one diagnose run: the time window, the output
//! path, the scrub toggle, and a test clock injection point.

use std::path::PathBuf;
use std::time::SystemTime;

/// Inputs to [`crate::collect`]. All fields have explicit defaults so a
/// caller can pass `Options::default()` for the common case
/// (last 7 days, scrub on, default output path, real clock).
#[derive(Debug, Clone)]
pub struct Options {
    /// Copy `bookrack.log.YYYY-MM-DD` files whose date is within this
    /// many days of `now`. Also filters `mcp_tool_calls`,
    /// `book_pipeline_audit`, and `metadata_audit` to rows whose
    /// timestamp is `>= now - days`.
    pub days: u32,
    /// When `true`, apply the scrubber to every string written into
    /// the bundle. When `false`, paths and book titles ride through
    /// verbatim — appropriate only for local-use bundles.
    pub scrub: bool,
    /// Output path for the resulting `.tar.gz`. `None` falls back to
    /// `<data_dir>/diagnostics/diagnose-<unix_ms>.tar.gz`.
    pub out: Option<PathBuf>,
    /// Test-only clock injection. `None` means [`SystemTime::now()`].
    pub now: Option<SystemTime>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            days: DEFAULT_DAYS,
            scrub: true,
            out: None,
            now: None,
        }
    }
}

/// Default time window: seven days of logs and audit rows.
pub const DEFAULT_DAYS: u32 = 7;
