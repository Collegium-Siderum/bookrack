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
// setting: daemon.nofile_target
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

/// Share of the soft limit, in percent, at or above which the open
/// descriptor count grades as [`FdPressure::High`].
// setting: daemon.fd_pressure_percent
pub const FD_PRESSURE_PERCENT: u64 = 75;

/// How close the open descriptor count is to the soft limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FdPressure {
    /// Below [`FD_PRESSURE_PERCENT`] of the soft limit, or no limit.
    Normal,
    /// At or above [`FD_PRESSURE_PERCENT`] of the soft limit.
    High,
    /// The open descriptors could not be counted.
    Unknown,
}

/// This process's open descriptors against its soft `RLIMIT_NOFILE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FdUsage {
    /// Descriptors open in this process; `None` when they could not be
    /// counted.
    pub open: Option<u64>,
    /// The soft limit in effect; `None` meaning unlimited.
    pub soft_limit: Option<u64>,
    /// The grade [`FdUsage::grade`] gives the two values above.
    pub pressure: FdPressure,
}

impl FdUsage {
    /// Grade an open count against a soft limit.
    pub fn grade(open: Option<u64>, soft_limit: Option<u64>) -> Self {
        let pressure = match (open, soft_limit) {
            (None, _) => FdPressure::Unknown,
            (Some(_), None) => FdPressure::Normal,
            (Some(open), Some(soft)) => {
                if u128::from(open) * 100 >= u128::from(soft) * u128::from(FD_PRESSURE_PERCENT) {
                    FdPressure::High
                } else {
                    FdPressure::Normal
                }
            }
        };
        Self {
            open,
            soft_limit,
            pressure,
        }
    }

    /// Count this process's open descriptors and read its soft limit.
    pub fn current() -> Self {
        Self::grade(count_open_fds().ok(), soft_nofile())
    }
}

/// The soft `RLIMIT_NOFILE` in effect, `None` meaning unlimited.
#[cfg(unix)]
pub fn soft_nofile() -> Option<u64> {
    rustix::process::getrlimit(rustix::process::Resource::Nofile).current
}

/// Non-Unix stub: the platform has no `RLIMIT_NOFILE`.
#[cfg(not(unix))]
pub fn soft_nofile() -> Option<u64> {
    None
}

/// Count the descriptors open in this process by listing `/dev/fd`,
/// which on Linux resolves to `/proc/self/fd`. The listing holds a
/// descriptor of its own while it is read; that one is not counted.
#[cfg(unix)]
pub fn count_open_fds() -> std::io::Result<u64> {
    let mut entries = 0u64;
    for entry in std::fs::read_dir("/dev/fd")? {
        entry?;
        entries += 1;
    }
    Ok(entries.saturating_sub(1))
}

/// Non-Unix stub: there is no per-process descriptor listing to read.
#[cfg(not(unix))]
pub fn count_open_fds() -> std::io::Result<u64> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grade_turns_high_at_the_pressure_share_of_the_soft_limit() {
        // 75% of 8192 is 6144 and of 48 is 36: the boundary is inclusive.
        assert_eq!(
            FdUsage::grade(Some(6143), Some(8192)).pressure,
            FdPressure::Normal
        );
        assert_eq!(
            FdUsage::grade(Some(6144), Some(8192)).pressure,
            FdPressure::High
        );
        assert_eq!(
            FdUsage::grade(Some(35), Some(48)).pressure,
            FdPressure::Normal
        );
        assert_eq!(
            FdUsage::grade(Some(36), Some(48)).pressure,
            FdPressure::High
        );
        assert_eq!(
            FdUsage::grade(Some(60), Some(48)).pressure,
            FdPressure::High
        );
    }

    #[test]
    fn an_unlimited_soft_limit_is_never_high_and_an_uncounted_one_is_unknown() {
        assert_eq!(
            FdUsage::grade(Some(u64::MAX), None).pressure,
            FdPressure::Normal
        );
        assert_eq!(
            FdUsage::grade(None, Some(8192)).pressure,
            FdPressure::Unknown
        );
        assert_eq!(FdUsage::grade(None, None).pressure, FdPressure::Unknown);
    }

    #[test]
    fn the_grade_keeps_the_values_it_was_given() {
        let usage = FdUsage::grade(Some(412), Some(8192));
        assert_eq!((usage.open, usage.soft_limit), (Some(412), Some(8192)));
    }

    #[test]
    fn the_usage_serialises_with_lowercase_pressure_and_null_for_no_limit() {
        let value = serde_json::to_value(FdUsage::grade(Some(10), None)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "open": 10, "soft_limit": null, "pressure": "normal" })
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_count_follows_descriptors_this_process_opens_and_closes() {
        const OPENED: u64 = 64;
        // Other tests in the same process may open or close a few
        // descriptors meanwhile; the window absorbs that, not an
        // off-by-one in the count, which shows on every reading.
        const SLACK: u64 = 8;
        raise_nofile().expect("raise");
        let dir = tempfile::tempdir().expect("tempdir");
        let before = count_open_fds().expect("count before");
        let files: Vec<std::fs::File> = (0..OPENED)
            .map(|i| std::fs::File::create(dir.path().join(format!("f{i}"))).expect("create"))
            .collect();
        let during = count_open_fds().expect("count during");
        drop(files);
        let after = count_open_fds().expect("count after");
        assert!(
            during.abs_diff(before + OPENED) <= SLACK,
            "opening {OPENED} files moved the count from {before} to {during}"
        );
        assert!(
            after.abs_diff(during - OPENED) <= SLACK,
            "closing {OPENED} files moved the count from {during} to {after}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_count_matches_a_probe_of_every_descriptor_number() {
        // The probe stats `/dev/fd/<n>` one number at a time, which
        // opens nothing, so it sees exactly the descriptors open around
        // it. A count that kept the listing's own descriptor reads one
        // more on every attempt; a reading disturbed by another test
        // opening or closing a file meanwhile shows as two probes that
        // disagree, and is taken again.
        fn probe() -> u64 {
            let ceiling = soft_nofile().unwrap_or(65_536).min(65_536);
            (0..ceiling)
                .filter(|n| std::fs::symlink_metadata(format!("/dev/fd/{n}")).is_ok())
                .count() as u64
        }
        let mut readings = Vec::new();
        for _ in 0..20 {
            let first = probe();
            let counted = count_open_fds().expect("count");
            let second = probe();
            if first == second {
                assert_eq!(
                    counted, first,
                    "the listing counted {counted}; probing every number found {first}"
                );
                return;
            }
            readings.push((first, counted, second));
        }
        panic!("no undisturbed reading in 20 attempts: {readings:?}");
    }

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
