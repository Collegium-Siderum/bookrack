// SPDX-License-Identifier: Apache-2.0

//! Pre-flight check that refuses a daemon-routed command when the
//! CLI's explicit library selection disagrees with the library a
//! running daemon is serving.
//!
//! The control-plane architecture is "one daemon owns one library":
//! once a daemon is up, every CLI command that routes through it
//! (`verify`, `exec`, `library.*` reads, ingest, ...) acts on the
//! daemon's library, not on whatever `--data-dir` / `--library` /
//! `BOOKRACK_DATA_DIR` the per-invocation environment expresses. The
//! pre-flight makes that takeover non-silent: it compares the
//! invoking shell's intent against the session lock's recorded
//! `data_dir` / `library_name`, and bails with a typed
//! [`BookrackCliError::LibraryMismatch`] when they differ so the
//! caller never has the daemon quietly acting on the "wrong" library
//! behind a flag that looks like it switched.
//!
//! The check is silent in every "cannot reliably compare" case:
//!   * no explicit selection was given (env / flag both unset)
//!   * no session lock is present (no daemon, or a torn-down one)
//!   * the lock predates the identity fields and carries neither
//!     `data_dir=` nor `library_name=`
//!   * the side of the comparison the intent expresses (path vs name)
//!     is the side the lock lacks
//!
//! Local-resolves commands (`run`, `init`, `doctor`, `audit-profile`,
//! `index-profile`, `distill`, `runs`, and the offline `libraries` verbs —
//! `default`/`detect`/`scan`/`add`/`register`/`remove`/`config`) bypass
//! this check entirely — for them the flag is a real switch into a
//! different data root, or an offline registry read/write, not an
//! assertion about the running daemon.

use std::path::{Path, PathBuf};

use bookrack_cli::error::BookrackCliError;
use bookrack_config::{DATA_DIR_ENV, LibrarySelection};
use bookrack_session::{LockInfo, peek_lock, resolve_runtime_dir, tty_lock_name};

/// Compare the CLI's explicit selection against the running session
/// (if any) and return [`BookrackCliError::LibraryMismatch`] when
/// they disagree. See the module-level doc for the silent-fallthrough
/// cases.
pub fn enforce_selection_mismatch(selection: &LibrarySelection) -> Result<(), BookrackCliError> {
    let env = std::env::var(DATA_DIR_ENV).ok();
    let Some(intent) = resolve_intent(selection, env.as_deref()) else {
        return Ok(());
    };
    let Some(lock) = read_lock_info() else {
        return Ok(());
    };
    if !is_mismatch(&intent, &lock) {
        return Ok(());
    }
    Err(BookrackCliError::LibraryMismatch {
        intent: render_intent(&intent),
        running: render_lock_target(&lock),
    })
}

/// The library the invoking shell appears to be asking for, distilled
/// from `--data-dir`, `--library`, and `BOOKRACK_DATA_DIR` in the
/// precedence the config crate documents.
#[derive(Debug, PartialEq, Eq)]
enum Intent {
    /// A path, from `--data-dir` or `BOOKRACK_DATA_DIR`. Compared
    /// against the lock's `data_dir=` after best-effort
    /// canonicalisation on both sides.
    Path(PathBuf),
    /// A registry name, from `--library`. Compared against the
    /// lock's `library_name=` byte-for-byte.
    Name(String),
}

fn resolve_intent(selection: &LibrarySelection, env_data_dir: Option<&str>) -> Option<Intent> {
    if let Some(p) = &selection.data_dir {
        return Some(Intent::Path(p.clone()));
    }
    if let Some(n) = &selection.library {
        return Some(Intent::Name(n.clone()));
    }
    if let Some(p) = env_data_dir.filter(|s| !s.is_empty()) {
        return Some(Intent::Path(PathBuf::from(p)));
    }
    None
}

fn read_lock_info() -> Option<LockInfo> {
    let runtime_dir = resolve_runtime_dir(None).ok()?;
    let lock_path = runtime_dir.join(tty_lock_name());
    peek_lock(&lock_path).ok().flatten()
}

fn is_mismatch(intent: &Intent, lock: &LockInfo) -> bool {
    match intent {
        Intent::Path(want) => match lock.data_dir.as_deref() {
            Some(have) => !same_path(want, have),
            None => false,
        },
        Intent::Name(want) => match lock.library_name.as_deref() {
            Some(have) => want != have,
            None => false,
        },
    }
}

/// Best-effort canonical-path comparison. Falls back to a raw
/// `PathBuf` equality when either side fails to canonicalise (typical
/// reason: the intent path does not exist on disk — which is itself a
/// reliable mismatch signal).
fn same_path(want: &Path, have: &Path) -> bool {
    let want_c = std::fs::canonicalize(want).unwrap_or_else(|_| want.to_path_buf());
    let have_c = std::fs::canonicalize(have).unwrap_or_else(|_| have.to_path_buf());
    want_c == have_c
}

fn render_intent(intent: &Intent) -> String {
    match intent {
        Intent::Path(p) => p.display().to_string(),
        Intent::Name(n) => format!("library {n}"),
    }
}

fn render_lock_target(lock: &LockInfo) -> String {
    match (&lock.data_dir, &lock.library_name) {
        (Some(p), Some(n)) => format!("{} (library {n})", p.display()),
        (Some(p), None) => p.display().to_string(),
        (None, Some(n)) => format!("library {n}"),
        (None, None) => "<unknown library>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_prefers_explicit_data_dir_over_env() {
        let selection = LibrarySelection {
            data_dir: Some(PathBuf::from("/flag")),
            library: None,
        };
        let intent = resolve_intent(&selection, Some("/env")).unwrap();
        assert_eq!(intent, Intent::Path(PathBuf::from("/flag")));
    }

    #[test]
    fn intent_prefers_library_flag_over_env() {
        let selection = LibrarySelection {
            data_dir: None,
            library: Some("named".into()),
        };
        let intent = resolve_intent(&selection, Some("/env")).unwrap();
        assert_eq!(intent, Intent::Name("named".into()));
    }

    #[test]
    fn intent_falls_through_to_env_when_no_flags() {
        let selection = LibrarySelection::default();
        let intent = resolve_intent(&selection, Some("/env")).unwrap();
        assert_eq!(intent, Intent::Path(PathBuf::from("/env")));
    }

    #[test]
    fn intent_is_none_when_nothing_is_set() {
        let selection = LibrarySelection::default();
        assert!(resolve_intent(&selection, None).is_none());
    }

    #[test]
    fn intent_treats_empty_env_as_unset() {
        let selection = LibrarySelection::default();
        assert!(resolve_intent(&selection, Some("")).is_none());
    }

    #[test]
    fn mismatch_when_paths_differ() {
        let lock = LockInfo {
            pid: 1,
            mcp: "disabled".into(),
            control_sock: None,
            data_dir: Some(PathBuf::from("/served")),
            library_name: None,
        };
        assert!(is_mismatch(&Intent::Path(PathBuf::from("/asked")), &lock));
    }

    #[test]
    fn no_mismatch_when_paths_match() {
        let lock = LockInfo {
            pid: 1,
            mcp: "disabled".into(),
            control_sock: None,
            data_dir: Some(PathBuf::from("/same")),
            library_name: None,
        };
        assert!(!is_mismatch(&Intent::Path(PathBuf::from("/same")), &lock));
    }

    #[test]
    fn silent_when_lock_has_no_data_dir() {
        let lock = LockInfo {
            pid: 1,
            mcp: "disabled".into(),
            control_sock: None,
            data_dir: None,
            library_name: None,
        };
        assert!(!is_mismatch(&Intent::Path(PathBuf::from("/asked")), &lock));
    }

    #[test]
    fn mismatch_when_library_names_differ() {
        let lock = LockInfo {
            pid: 1,
            mcp: "disabled".into(),
            control_sock: None,
            data_dir: None,
            library_name: Some("served".into()),
        };
        assert!(is_mismatch(&Intent::Name("asked".into()), &lock));
    }

    #[test]
    fn no_mismatch_when_library_names_match() {
        let lock = LockInfo {
            pid: 1,
            mcp: "disabled".into(),
            control_sock: None,
            data_dir: None,
            library_name: Some("same".into()),
        };
        assert!(!is_mismatch(&Intent::Name("same".into()), &lock));
    }

    #[test]
    fn silent_when_lock_has_no_library_name_and_intent_is_a_name() {
        let lock = LockInfo {
            pid: 1,
            mcp: "disabled".into(),
            control_sock: None,
            data_dir: Some(PathBuf::from("/served")),
            library_name: None,
        };
        assert!(!is_mismatch(&Intent::Name("asked".into()), &lock));
    }

    #[test]
    fn render_lock_target_handles_every_field_combo() {
        let with_both = LockInfo {
            pid: 1,
            mcp: "disabled".into(),
            control_sock: None,
            data_dir: Some(PathBuf::from("/p")),
            library_name: Some("n".into()),
        };
        assert_eq!(render_lock_target(&with_both), "/p (library n)");

        let path_only = LockInfo {
            data_dir: Some(PathBuf::from("/p")),
            library_name: None,
            ..with_both.clone()
        };
        assert_eq!(render_lock_target(&path_only), "/p");

        let name_only = LockInfo {
            data_dir: None,
            library_name: Some("n".into()),
            ..with_both.clone()
        };
        assert_eq!(render_lock_target(&name_only), "library n");

        let empty = LockInfo {
            data_dir: None,
            library_name: None,
            ..with_both
        };
        assert_eq!(render_lock_target(&empty), "<unknown library>");
    }
}
