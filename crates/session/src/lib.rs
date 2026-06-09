// SPDX-License-Identifier: Apache-2.0

//! Session-scoped tty lock shared by every bookrack process that holds
//! a database write handle.
//!
//! `bookrack run` and the headless `bookrack-mcp` both compete for the
//! same lock file under the runtime directory, so the operator cannot
//! accidentally point two writers at the same on-disk catalog or
//! corpus. Readers that go through the MCP HTTP surface (`bookrack
//! exec`) never take the lock; they read its contents to discover the
//! running session and reach it over the network.
//!
//! The lock is advisory `flock`-style: the OS releases it when the
//! [`File`] handle drops, so a crashed process leaves no stale lock.
//! Stale *content* — a pid or MCP address from a previous run — is
//! tolerated and overwritten by the next successful acquire.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use fs2::FileExt;

/// Environment variable naming the session runtime directory (lock
/// file, REPL history). Optional; the default is platform-conventional.
pub const RUNTIME_DIR_ENV: &str = "BOOKRACK_RUNTIME_DIR";

const TTY_LOCK_NAME_STR: &str = "bookrack.tty.lock";

/// File name of the session-scoped lock under the runtime directory.
/// Exposed so siblings (the `cli` REPL, the headless `mcp` binary,
/// `bookrack exec`) discover the active session through the same path.
pub fn tty_lock_name() -> &'static str {
    TTY_LOCK_NAME_STR
}

/// Resolve the runtime directory. Precedence: explicit override, then
/// [`RUNTIME_DIR_ENV`], then platform default.
pub fn resolve_runtime_dir(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    if let Ok(v) = std::env::var(RUNTIME_DIR_ENV)
        && !v.trim().is_empty()
    {
        return Ok(PathBuf::from(v));
    }
    platform_runtime_dir()
}

/// Platform-conventional fallback for the runtime directory.
///
/// Linux prefers `$XDG_RUNTIME_DIR` (ephemeral, tmpfs-backed) and
/// falls back to the cache dir (`$XDG_CACHE_HOME` or `~/.cache`).
/// macOS and Windows use the cache dir directly (`~/Library/Caches`
/// and `%LOCALAPPDATA%`).
fn platform_runtime_dir() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Some(dir) = dirs::runtime_dir() {
            return Ok(dir.join("bookrack"));
        }
    }
    let cache = dirs::cache_dir().ok_or_else(|| {
        anyhow!(
            "cannot find a platform cache directory for the bookrack runtime dir; \
             set {RUNTIME_DIR_ENV} to an absolute path"
        )
    })?;
    Ok(cache.join("bookrack"))
}

/// Drop guard for the session's tty lock.
///
/// The OS releases the advisory flock when [`File`] drops, so a
/// crashed process leaves no stale lock — only stale content (the
/// recorded pid and MCP address) that the next acquirer overwrites.
/// Intentionally not `Drop`-implemented because the underlying file
/// handle's drop is the release.
pub struct TtyLock {
    #[allow(dead_code)]
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl TtyLock {
    /// Acquire the session lock at `path`, writing the running pid,
    /// the chosen MCP address (or `disabled`), and optionally the
    /// control-plane socket path into it so other tools —
    /// `bookrack exec`, `bookrack doctor` — can find the live session
    /// and reach its control plane.
    ///
    /// The `control_sock` argument is `None` when the caller does not
    /// yet know the bind path at acquire time; callers can attach the
    /// path later through [`TtyLock::record_control_sock`].
    ///
    /// Returns an error containing the conflicting session's recorded
    /// pid and MCP address when another process already holds the
    /// lock; the file content is read after the conflict, so a stale
    /// pid from a crashed predecessor does not show up here (the next
    /// successful acquire writes fresh content).
    pub fn acquire(
        path: &Path,
        pid: u32,
        mcp_addr: &str,
        control_sock: Option<&Path>,
    ) -> Result<TtyLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open session lock {}", path.display()))?;
        file.try_lock_exclusive().map_err(|err| {
            let existing = std::fs::read_to_string(path).unwrap_or_default();
            let detail = existing.trim();
            if detail.is_empty() {
                anyhow!(
                    "bookrack session already running, lock held at {}: {err}",
                    path.display()
                )
            } else {
                anyhow!(
                    "bookrack session already running ({}), lock held at {}: {err}",
                    detail.replace('\n', ", "),
                    path.display()
                )
            }
        })?;
        let mut owned = file;
        owned.set_len(0).context("truncate session lock contents")?;
        write!(owned, "pid={pid}\nmcp={mcp_addr}\n").context("write session lock contents")?;
        if let Some(sock) = control_sock {
            writeln!(owned, "control_sock={}", sock.display())
                .context("write session lock control_sock line")?;
        }
        Ok(TtyLock {
            file: owned,
            path: path.to_path_buf(),
        })
    }

    /// Append a `control_sock=<path>` line to the lock file. Used by
    /// callers that bind the control-plane socket after the lock is
    /// already held, so the recorded path matches the listener that
    /// actually came up.
    pub fn record_control_sock(&mut self, control_sock: &Path) -> Result<()> {
        writeln!(self.file, "control_sock={}", control_sock.display())
            .context("append session lock control_sock line")
    }
}

/// Snapshot of a session lock file's contents. Returned by
/// [`peek_lock`] for callers that want to discover a live session
/// (its pid, MCP listener label, and control-plane socket path)
/// without taking the lock themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    pub pid: u32,
    pub mcp: String,
    pub control_sock: Option<PathBuf>,
}

/// Read the session lock at `path` without acquiring it.
///
/// Returns `Ok(None)` when the file does not exist. Returns `Err`
/// when the file cannot be read, or when its contents are missing
/// the required `pid=` / `mcp=` lines or carry a `pid` value that
/// is not a `u32`. The `control_sock=` line is optional: a lock
/// file written by a daemon that predates Phase 1, or one whose
/// daemon ran without a control-plane listener, parses cleanly with
/// `control_sock: None`.
pub fn peek_lock(path: &Path) -> Result<Option<LockInfo>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("read session lock at {}", path.display()))
            );
        }
    };
    parse_lock(&raw, path).map(Some)
}

fn parse_lock(raw: &str, source: &Path) -> Result<LockInfo> {
    let mut pid: Option<u32> = None;
    let mut mcp: Option<String> = None;
    let mut control_sock: Option<PathBuf> = None;
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("pid=") {
            pid = Some(value.parse::<u32>().with_context(|| {
                format!(
                    "parse `pid=` line in session lock at {}: not a u32",
                    source.display()
                )
            })?);
        } else if let Some(value) = line.strip_prefix("mcp=") {
            mcp = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("control_sock=") {
            control_sock = Some(PathBuf::from(value));
        }
    }
    let pid = pid.ok_or_else(|| {
        anyhow!(
            "session lock at {} missing required `pid=` line",
            source.display()
        )
    })?;
    let mcp = mcp.ok_or_else(|| {
        anyhow!(
            "session lock at {} missing required `mcp=` line",
            source.display()
        )
    })?;
    Ok(LockInfo {
        pid,
        mcp,
        control_sock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    #[test]
    fn tty_lock_blocks_a_second_acquirer_until_dropped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let lock1 = TtyLock::acquire(&path, 1234, "127.0.0.1:8765", None).unwrap();

        let second = TtyLock::acquire(&path, 5678, "127.0.0.1:8765", None);
        assert!(second.is_err(), "expected second acquire to fail");

        drop(lock1);
        let _lock2 = TtyLock::acquire(&path, 9999, "127.0.0.1:8765", None)
            .expect("re-acquire after drop must succeed");
    }

    #[test]
    fn tty_lock_conflict_message_surfaces_pid_and_mcp_addr() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let _lock1 = TtyLock::acquire(&path, 7777, "127.0.0.1:9999", None).unwrap();
        let err = match TtyLock::acquire(&path, 8888, "127.0.0.1:9999", None) {
            Ok(_) => panic!("expected lock conflict"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("7777"), "pid not in error: {msg}");
        assert!(msg.contains("127.0.0.1:9999"), "mcp not in error: {msg}");
        assert!(msg.contains("already running"));
    }

    #[test]
    fn tty_lock_truncates_stale_content_on_acquire() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        std::fs::write(&path, "pid=stale\nmcp=stale\nextra-line\n").unwrap();
        let _lock = TtyLock::acquire(&path, 4242, "disabled", None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("pid=4242"),
            "fresh pid missing: {content:?}"
        );
        assert!(
            content.contains("mcp=disabled"),
            "fresh mcp missing: {content:?}"
        );
        assert!(
            !content.contains("stale"),
            "stale content not truncated: {content:?}"
        );
    }

    #[test]
    fn tty_lock_writes_control_sock_when_provided_at_acquire() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let sock = dir.path().join("control.sock");
        let _lock = TtyLock::acquire(&path, 11, "127.0.0.1:1", Some(&sock)).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let sock_line = format!("control_sock={}", sock.display());
        assert!(
            content.contains(&sock_line),
            "control_sock line missing: {content:?}"
        );
        assert!(content.contains("pid=11"));
        assert!(content.contains("mcp=127.0.0.1:1"));
    }

    #[test]
    fn tty_lock_omits_control_sock_line_when_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let _lock = TtyLock::acquire(&path, 22, "disabled", None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("control_sock="),
            "unexpected control_sock line: {content:?}"
        );
    }

    #[test]
    fn record_control_sock_appends_a_line_to_the_held_lock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let mut lock = TtyLock::acquire(&path, 33, "disabled", None).unwrap();
        let sock = dir.path().join("ctrl.sock");
        lock.record_control_sock(&sock).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let sock_line = format!("control_sock={}", sock.display());
        assert!(
            content.contains(&sock_line),
            "control_sock line missing after record: {content:?}"
        );
    }

    #[test]
    fn resolve_runtime_dir_prefers_explicit_override() {
        let path = PathBuf::from("/tmp/bookrack-test-override");
        assert_eq!(resolve_runtime_dir(Some(&path)).unwrap(), path);
    }

    #[test]
    fn peek_lock_returns_none_when_file_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.lock");
        assert!(peek_lock(&path).unwrap().is_none());
    }

    #[test]
    fn peek_lock_parses_pid_mcp_and_optional_control_sock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bookrack.tty.lock");
        std::fs::write(
            &path,
            "pid=4242\nmcp=127.0.0.1:8765\ncontrol_sock=/tmp/x.sock\n",
        )
        .unwrap();
        let info = peek_lock(&path).unwrap().unwrap();
        assert_eq!(info.pid, 4242);
        assert_eq!(info.mcp, "127.0.0.1:8765");
        assert_eq!(info.control_sock.as_deref(), Some(Path::new("/tmp/x.sock")));
    }

    #[test]
    fn peek_lock_tolerates_unknown_lines_and_omitted_control_sock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bookrack.tty.lock");
        std::fs::write(&path, "pid=1\nfuture_key=ignored\nmcp=disabled\n").unwrap();
        let info = peek_lock(&path).unwrap().unwrap();
        assert_eq!(info.pid, 1);
        assert_eq!(info.mcp, "disabled");
        assert!(info.control_sock.is_none());
    }

    #[test]
    fn peek_lock_errors_when_pid_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bookrack.tty.lock");
        std::fs::write(&path, "mcp=disabled\n").unwrap();
        assert!(peek_lock(&path).is_err());
    }

    #[test]
    fn peek_lock_errors_when_pid_not_a_u32() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bookrack.tty.lock");
        std::fs::write(&path, "pid=not-a-number\nmcp=disabled\n").unwrap();
        assert!(peek_lock(&path).is_err());
    }
}
