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

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bookrack_config::Config;

pub use options::{DEFAULT_DAYS, Options};
pub use scrub::Scrubber;

bookrack_core::fixed_settings! {
    owner = "diagnose";
    "diagnose.days_default" = options::DEFAULT_DAYS,
        "days of logs and audit rows a bundle covers when no window is named",
        acts on "diagnose";
    "diagnose.gzip_level" = tarball::GZIP_LEVEL,
        "compression level the bundle's gzip wrapper is written at",
        acts on "how large a diagnose bundle is and how long it takes to write";
    "diagnose.intake_head_max" = collectors::catalog::INTAKE_HEAD_CAP,
        "intake rows captured from the head of the table into a bundle",
        acts on "diagnose";
    "diagnose.recent_rows_max" = collectors::catalog::RECENT_ROW_CAP,
        "recent rows of each observability table captured into a bundle",
        acts on "diagnose";
}

/// Why a [`collect`] call failed. Recoverable per-collector errors are
/// swallowed and surfaced as missing manifest entries instead — only
/// fatal IO and serialization failures land here.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DiagnoseError {
    /// A filesystem error.
    #[error("io")]
    Io(#[from] std::io::Error),
    /// A catalog read failed.
    #[error("catalog")]
    Catalog(#[from] bookrack_catalog::CatalogError),
    /// A corpus read failed.
    #[error("corpus")]
    Corpus(#[from] bookrack_corpus::CorpusError),
    /// JSON serialization failed.
    #[error("json")]
    Json(#[from] serde_json::Error),
}

/// A fallible diagnose op.
pub type Result<T> = std::result::Result<T, DiagnoseError>;

/// Where the home-directory prefix consumed by scrub rule 3 came from.
///
/// Recorded because the three cases are not equally safe: an
/// unresolved home directory leaves rule 3 with nothing to match, so
/// a home path outside the OS user-root patterns of rule 1 (`/root`,
/// a container's `HOME`, a Windows profile off the system drive)
/// reaches the bundle verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeSource {
    /// The `HOME` variable carried a non-empty path.
    Env,
    /// `HOME` was absent or empty and the platform lookup supplied the
    /// path — the passwd database on unix, the profile known folder on
    /// Windows.
    Platform,
    /// Neither source yielded a path.
    Unresolved,
}

/// The `scrub_gaps` token naming an unresolved home directory.
pub const SCRUB_GAP_HOME_DIR: &str = "home_dir";

/// The `scrub_gaps` token naming a home directory taken from `HOME`
/// that does not exist on this machine.
///
/// The redaction ran, so it is not the same shortfall as
/// [`SCRUB_GAP_HOME_DIR`]: rule 3 folded a prefix, just not one this
/// host has. Whatever the real home directory is, it went through
/// unredacted except where the generic user-root patterns of rule 1
/// happened to catch it.
pub const SCRUB_GAP_HOME_DIR_UNVERIFIED: &str = "home_dir_unverified";

/// Summary returned by [`collect`].
#[derive(Debug)]
pub struct CollectReport {
    /// Where the tarball was written.
    pub out_path: PathBuf,
    /// Number of files inside the archive.
    pub files: usize,
    /// Whether the scrubber ran (mirrors `opts.scrub`).
    pub scrubbed: bool,
    /// Redactions the scrubber could not perform, as written to the
    /// manifest. Empty when every rule had its input; a front end
    /// warns on a non-empty list, because the bundle is less redacted
    /// than `scrubbed: true` alone suggests.
    pub scrub_gaps: Vec<&'static str>,
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

    let (scrubber, home_source, home_prefix) = if opts.scrub {
        let (home, source) = resolve_home(std::env::var_os("HOME"), dirs::home_dir);
        let data = bookrack_audit_profile::AuditData::load_from(&cfg.audit_rules_dir())
            .unwrap_or_else(|_| bookrack_audit_profile::AuditData::default_data());
        let scrubber = Scrubber::new(
            Some(cfg.data_dir()),
            home.as_deref(),
            data.scrub_book_extensions,
        );
        (scrubber, Some(source), home)
    } else {
        (Scrubber::passthrough(), None, None)
    };
    let gaps = scrub_gaps(home_source, home_prefix.as_deref(), |p| p.is_dir());

    let since = since_ts(now, opts.days);

    collectors::env::collect(cfg, opts, now, home_source, &bundle_dir, &scrubber)?;
    collectors::crashes::collect(cfg, &bundle_dir, &scrubber)?;
    collectors::logs::collect(cfg, opts, now, &bundle_dir, &scrubber)?;
    collectors::catalog::collect(cfg, &since, &bundle_dir, &scrubber)?;
    collectors::corpus::collect(cfg, &bundle_dir)?;
    collectors::vectors::collect(cfg, &bundle_dir)?;

    let manifest = manifest::build(opts, &bundle_dir, now, gaps)?;
    let scrubbed = manifest.scrubbed;
    let files = manifest.files.len();
    let scrub_gaps = manifest.scrub_gaps.clone();
    manifest::write(&bundle_dir, &manifest)?;

    let out_path = resolve_out_path(cfg, opts, unix_ms);
    tarball::write_bundle(&bundle_dir, &out_path)?;

    Ok(CollectReport {
        out_path,
        files,
        scrubbed,
        scrub_gaps,
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

/// Resolve the home-directory prefix that scrub rule 3 substitutes.
///
/// `env_home` is the raw `HOME` value; an empty one counts as absent.
/// `platform` is the platform-level lookup consulted when the
/// environment carries nothing, so a process started without `HOME`
/// still redacts its own home directory.
fn resolve_home(
    env_home: Option<OsString>,
    platform: impl FnOnce() -> Option<PathBuf>,
) -> (Option<PathBuf>, HomeSource) {
    if let Some(h) = env_home.filter(|h| !h.is_empty()) {
        return (Some(PathBuf::from(h)), HomeSource::Env);
    }
    match platform() {
        Some(p) => (Some(p), HomeSource::Platform),
        None => (None, HomeSource::Unresolved),
    }
}

/// Name every redaction the scrubber could not perform, or performed
/// against a prefix worth doubting. `home` of `None` means the
/// scrubber did not run at all, which `scrubbed: false` already
/// states — a bundle nobody redacted has no partial coverage to
/// report.
///
/// `prefix` is the path rule 3 substitutes and `is_dir` decides
/// whether it names a directory here, injected so the judgement is
/// testable without a home directory to arrange.
fn scrub_gaps(
    home: Option<HomeSource>,
    prefix: Option<&Path>,
    is_dir: impl Fn(&Path) -> bool,
) -> Vec<&'static str> {
    match home {
        Some(HomeSource::Unresolved) => vec![SCRUB_GAP_HOME_DIR],
        // Only the environment's value is doubted, and only for
        // naming nothing. `HOME` is writable by anyone who can start
        // the process — a shell export, a `.env` two directories up —
        // and a wrong one leaves rule 3 folding a prefix no path in
        // the bundle begins with, silently. A `HOME` pointing at some
        // other directory that does exist is indistinguishable from a
        // correct one without a passwd lookup, so it is not claimed
        // here; what is caught is the shape a mistyped or stale value
        // usually takes.
        Some(HomeSource::Env) if prefix.is_some_and(|p| !is_dir(p)) => {
            vec![SCRUB_GAP_HOME_DIR_UNVERIFIED]
        }
        _ => Vec::new(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic home directory that matches none of the OS
    /// user-root patterns scrub rule 1 recognises, so a test using it
    /// exercises rule 3 alone.
    const FAKE_HOME: &str = "/a/profile";

    #[test]
    fn a_non_empty_home_variable_wins_over_the_platform_lookup() {
        let (path, source) = resolve_home(Some(OsString::from(FAKE_HOME)), || {
            Some(PathBuf::from("/other"))
        });
        assert_eq!(path, Some(PathBuf::from(FAKE_HOME)));
        assert_eq!(source, HomeSource::Env);
    }

    #[test]
    fn an_absent_home_variable_falls_back_to_the_platform_lookup() {
        let (path, source) = resolve_home(None, || Some(PathBuf::from(FAKE_HOME)));
        assert_eq!(path, Some(PathBuf::from(FAKE_HOME)));
        assert_eq!(source, HomeSource::Platform);
    }

    #[test]
    fn an_empty_home_variable_falls_back_to_the_platform_lookup() {
        let (path, source) = resolve_home(Some(OsString::new()), || Some(PathBuf::from(FAKE_HOME)));
        assert_eq!(path, Some(PathBuf::from(FAKE_HOME)));
        assert_eq!(source, HomeSource::Platform);
    }

    #[test]
    fn a_host_exposing_no_home_at_all_resolves_to_unresolved() {
        let (path, source) = resolve_home(None, || None);
        assert_eq!(path, None);
        assert_eq!(source, HomeSource::Unresolved);
    }

    /// A home directory that is there, from either source, is full
    /// coverage.
    #[test]
    fn a_home_directory_that_exists_reports_no_gap() {
        let home = Some(Path::new(FAKE_HOME));
        let exists = |_: &Path| true;
        assert_eq!(
            scrub_gaps(Some(HomeSource::Env), home, exists),
            Vec::<&str>::new()
        );
        assert_eq!(
            scrub_gaps(Some(HomeSource::Platform), home, exists),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn a_host_with_no_home_at_all_reports_the_unredacted_gap() {
        assert_eq!(
            scrub_gaps(Some(HomeSource::Unresolved), None, |_| false),
            vec![SCRUB_GAP_HOME_DIR]
        );
        // `--no-scrub`: nothing was redacted, so nothing is a gap.
        assert_eq!(scrub_gaps(None, None, |_| false), Vec::<&str>::new());
    }

    /// The case the environment opens: `HOME` names a path this host
    /// does not have, so rule 3 folds a prefix nothing in the bundle
    /// starts with, and the real home directory is left in the text.
    /// A manifest reporting no gap here tells the receiver the bundle
    /// is covered when it is not.
    #[test]
    fn a_home_from_the_environment_that_does_not_exist_reports_a_gap() {
        assert_eq!(
            scrub_gaps(Some(HomeSource::Env), Some(Path::new(FAKE_HOME)), |_| false),
            vec![SCRUB_GAP_HOME_DIR_UNVERIFIED],
        );
    }

    /// The platform lookup reads the passwd database, not a variable
    /// anyone can set, so a path it returns is not doubted for merely
    /// being absent — an unwritable or not-yet-created home is not the
    /// wrong prefix, it is the right one.
    #[test]
    fn a_platform_resolved_home_is_not_doubted_for_being_absent() {
        assert_eq!(
            scrub_gaps(
                Some(HomeSource::Platform),
                Some(Path::new(FAKE_HOME)),
                |_| false
            ),
            Vec::<&str>::new(),
        );
    }

    #[test]
    fn a_platform_resolved_home_still_reaches_the_scrubber() {
        // The end the fallback exists for: with no `HOME`, the home
        // path must still be substituted by rule 3, which rule 1 does
        // not cover for a home outside the OS user-root patterns.
        let (home, source) = resolve_home(None, || Some(PathBuf::from(FAKE_HOME)));
        assert_eq!(source, HomeSource::Platform);
        let scrubber = Scrubber::new(None, home.as_deref(), Vec::new());
        assert_eq!(
            scrubber.scrub_string(&format!("{FAKE_HOME}/.bookrackrc")),
            format!("{HOME_PLACEHOLDER}/.bookrackrc"),
        );
    }
}
