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
    /// Acquire the session lock at `path`, writing the running pid
    /// and the chosen MCP address (or `disabled`) into it so other
    /// tools — `bookrack exec`, `bookrack doctor` — can find the live
    /// session.
    ///
    /// Returns an error containing the conflicting session's recorded
    /// pid and MCP address when another process already holds the
    /// lock; the file content is read after the conflict, so a stale
    /// pid from a crashed predecessor does not show up here (the next
    /// successful acquire writes fresh content).
    pub fn acquire(path: &Path, pid: u32, mcp_addr: &str) -> Result<TtyLock> {
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
        Ok(TtyLock {
            file: owned,
            path: path.to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    #[test]
    fn tty_lock_blocks_a_second_acquirer_until_dropped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let lock1 = TtyLock::acquire(&path, 1234, "127.0.0.1:8765").unwrap();

        let second = TtyLock::acquire(&path, 5678, "127.0.0.1:8765");
        assert!(second.is_err(), "expected second acquire to fail");

        drop(lock1);
        let _lock2 = TtyLock::acquire(&path, 9999, "127.0.0.1:8765")
            .expect("re-acquire after drop must succeed");
    }

    #[test]
    fn tty_lock_conflict_message_surfaces_pid_and_mcp_addr() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let _lock1 = TtyLock::acquire(&path, 7777, "127.0.0.1:9999").unwrap();
        let err = match TtyLock::acquire(&path, 8888, "127.0.0.1:9999") {
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
        let _lock = TtyLock::acquire(&path, 4242, "disabled").unwrap();
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
    fn resolve_runtime_dir_prefers_explicit_override() {
        let path = PathBuf::from("/tmp/bookrack-test-override");
        assert_eq!(resolve_runtime_dir(Some(&path)).unwrap(), path);
    }
}
