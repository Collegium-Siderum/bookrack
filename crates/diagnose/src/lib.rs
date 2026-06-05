// SPDX-License-Identifier: Apache-2.0

//! bookrack-diagnose: build a forensic `tar.gz` bundle from a bookrack
//! data dir.
//!
//! [`collect`] is the one entry point. Given a [`bookrack_config::Config`]
//! and an [`Options`], it walks the data root, copies what it finds —
//! crash reports, rolling logs, intake heads, recent tool calls,
//! pipeline and metadata audit rows, the corpus stamps, and the
//! vectors metadata sidecar — through the [`scrub::Scrubber`] and into
//! a deterministic, gzip-wrapped tar archive.
//!
//! Collection is opportunistic: a missing logs directory or an empty
//! catalog table is normal, not an error. Only failures that prevent
//! the bundle from being written at all bubble up as
//! [`DiagnoseError`].

pub mod collectors;
pub mod manifest;
pub mod options;
pub mod scrub;
pub mod tarball;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bookrack_config::Config;

pub use options::{DEFAULT_DAYS, Options};
pub use scrub::Scrubber;

/// Why a [`collect`] call failed. Recoverable per-collector errors are
/// swallowed and surfaced as missing manifest entries instead — only
/// fatal IO and serialization failures land here.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DiagnoseError {
    /// A filesystem error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A catalog read failed.
    #[error("catalog: {0}")]
    Catalog(#[from] bookrack_catalog::CatalogError),
    /// A corpus read failed.
    #[error("corpus: {0}")]
    Corpus(#[from] bookrack_corpus::CorpusError),
    /// JSON serialization failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// A fallible diagnose op.
pub type Result<T> = std::result::Result<T, DiagnoseError>;

/// Summary returned by [`collect`].
#[derive(Debug)]
pub struct CollectReport {
    /// Where the tarball was written.
    pub out_path: PathBuf,
    /// Number of files inside the archive.
    pub files: usize,
    /// Whether the scrubber ran (mirrors `opts.scrub`).
    pub scrubbed: bool,
    /// The unix-ms timestamp embedded in the bundle name.
    pub generated_at_unix_ms: u128,
}

/// Build a diagnose bundle for `cfg` and return where it landed.
pub fn collect(cfg: &Config, opts: &Options) -> Result<CollectReport> {
    let now = opts.now.unwrap_or_else(SystemTime::now);
    let unix_ms = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let staging = tempfile::tempdir()?;
    let bundle_dir = staging.path().join(format!("diagnose-{unix_ms}"));
    std::fs::create_dir_all(&bundle_dir)?;

    let scrubber = if opts.scrub {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Scrubber::new(Some(cfg.data_dir()), home.as_deref())
    } else {
        Scrubber::passthrough()
    };

    let since = since_ts(now, opts.days);

    collectors::env::collect(cfg, opts, now, &bundle_dir, &scrubber)?;
    collectors::crashes::collect(cfg, &bundle_dir, &scrubber)?;
    collectors::logs::collect(cfg, opts, now, &bundle_dir, &scrubber)?;
    collectors::catalog::collect(cfg, &since, &bundle_dir, &scrubber)?;
    collectors::corpus::collect(cfg, &bundle_dir)?;
    collectors::vectors::collect(cfg, &bundle_dir)?;

    let manifest = manifest::build(opts, &bundle_dir, now)?;
    let scrubbed = manifest.scrubbed;
    let files = manifest.files.len();
    manifest::write(&bundle_dir, &manifest)?;

    let out_path = resolve_out_path(cfg, opts, unix_ms);
    tarball::write_bundle(&bundle_dir, &out_path)?;

    Ok(CollectReport {
        out_path,
        files,
        scrubbed,
        generated_at_unix_ms: unix_ms,
    })
}

fn resolve_out_path(cfg: &Config, opts: &Options, unix_ms: u128) -> PathBuf {
    if let Some(p) = &opts.out {
        return p.clone();
    }
    let dir = cfg.data_dir().join("diagnostics");
    dir.join(format!("diagnose-{unix_ms}.tar.gz"))
}

/// Return the ISO-8601 cutoff that catalog queries should treat as
/// "include rows from this point forward."
fn since_ts(now: SystemTime, days: u32) -> String {
    let days_secs = u64::from(days).saturating_mul(86_400);
    let cutoff = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(days_secs))
        .unwrap_or(0);
    manifest::iso8601_z(UNIX_EPOCH + std::time::Duration::from_secs(cutoff))
}

/// Re-export the public path placeholders so consumers (and the CLI's
/// human-readable output) can recognise them.
pub use scrub::{DATA_DIR_PLACEHOLDER, HOME_PLACEHOLDER, USER_PLACEHOLDER, VOL_PLACEHOLDER};
