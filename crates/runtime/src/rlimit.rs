// SPDX-License-Identifier: Apache-2.0

//! Process-wide `RLIMIT_NOFILE` introspection and raise.
//!
//! LanceDB fragment files, the SQLite stores, log sinks, and the
//! control / MCP listeners all hold descriptors concurrently. A batch
//! ingest exceeds the small default soft limit that GUI-launched
//! processes inherit (256 on macOS), while shell-launched processes
//! inherit the shell's higher limit — so the exhaustion only shows up
//! in the tray host. Both daemon hosts raise the soft limit once at
//! startup; `doctor` performs the same call so its row reports what a
//! daemon launched from this environment would actually run with.

/// Soft limit the daemon requests at startup. Sized for a large batch
/// ingest: hundreds of LanceDB fragment files between compactions plus
/// the fixed per-session descriptors, with headroom.
pub const NOFILE_TARGET: u64 = 8192;

/// Raise the soft `RLIMIT_NOFILE` to `min(hard, NOFILE_TARGET)`.
///
/// Returns the soft limit in effect after the call, `None` meaning
/// unlimited. A soft limit already at or above the target is left
/// untouched; the limit is never lowered.
#[cfg(unix)]
pub fn raise_nofile() -> std::io::Result<Option<u64>> {
    use rustix::process::{Resource, getrlimit, setrlimit};

    let mut limit = getrlimit(Resource::Nofile);
    let desired = match limit.maximum {
        Some(hard) => hard.min(NOFILE_TARGET),
        None => NOFILE_TARGET,
    };
    match limit.current {
        None => return Ok(None),
        Some(soft) if soft >= desired => return Ok(Some(soft)),
        Some(_) => {}
    }
    limit.current = Some(desired);
    setrlimit(Resource::Nofile, limit)?;
    Ok(Some(desired))
}

/// Non-Unix stub: the platform has no `RLIMIT_NOFILE`; report
/// unlimited so callers skip the warning path.
#[cfg(not(unix))]
pub fn raise_nofile() -> std::io::Result<Option<u64>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raise_is_idempotent_and_never_lowers() {
        let first = raise_nofile().expect("first raise");
        let second = raise_nofile().expect("second raise");
        assert_eq!(first, second);
        if let Some(soft) = second {
            assert!(soft > 0);
        }
    }
}
