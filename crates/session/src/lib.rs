// SPDX-License-Identifier: Apache-2.0

//! Cross-process locks guarding a bookrack session and the data root
//! it serves.
//!
//! Two locks with distinct jobs live here:
//!
//! - [`TtyLock`] at `<runtime_dir>/bookrack.tty.lock` — one daemon per
//!   runtime directory, plus the discovery lines (`pid=`, `mcp=`,
//!   `control_sock=`) other tools read to reach the live session.
//!   `bookrack run` and the headless `bookrack-mcp` compete for it, so
//!   the operator cannot accidentally point two writers at the same
//!   on-disk catalog or corpus. Readers that go through the MCP HTTP
//!   surface (`bookrack exec`) never take it.
//! - [`RootLock`] at `<data_root>/.bookrack.lock` — one writer per data
//!   root, whether that writer is a daemon serving the root or an
//!   offline command about to destroy it. Its contents are display-only
//!   and no reader may decide anything from them.
//!
//! Both are advisory `flock`-style: the OS releases them when the
//! [`File`] handle drops, so a crashed process leaves no stale lock.
//! Stale *content* — a pid or MCP address from a previous run — is
//! tolerated and overwritten by the next successful acquire.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use eyre::{Context, Result, eyre};
use fs2::FileExt;

/// Environment variable naming the session runtime directory (lock
/// file, REPL history). Optional; the default is platform-conventional.
pub const RUNTIME_DIR_ENV: &str = "BOOKRACK_RUNTIME_DIR";

const TTY_LOCK_NAME_STR: &str = "bookrack.tty.lock";

const ROOT_LOCK_NAME_STR: &str = ".bookrack.lock";

/// File name of the session-scoped lock under the runtime directory.
/// Exposed so siblings (the `cli` REPL, the headless `mcp` binary,
/// `bookrack exec`) discover the active session through the same path.
pub fn tty_lock_name() -> &'static str {
    TTY_LOCK_NAME_STR
}

/// Path of the data-root lock inside `root`.
///
/// A dot-prefixed file of its own: the lock never participates in an
/// atomic rewrite (`flock` follows the inode, and every atomically
/// rewritten file is replaced by `rename`), and the dot keeps it out of
/// data-root detection heuristics and content scans.
pub fn root_lock_path(root: &Path) -> PathBuf {
    root.join(ROOT_LOCK_NAME_STR)
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
        eyre!(
            "cannot find a platform cache directory for the bookrack runtime dir; \
             set {RUNTIME_DIR_ENV} to an absolute path"
        )
    })?;
    Ok(cache.join("bookrack"))
}

/// Drop guard for the session's tty lock.
///
/// Marker prefix of the error produced when the session lock is
/// already held. [`is_lock_conflict`] matches against it; both sides
/// live in this crate so the text has a single source.
const LOCK_CONFLICT_MARKER: &str = "bookrack session already running";

/// Report whether `err` (or any of its causes) is the lock-conflict
/// error produced by [`TtyLock::acquire`] when another process holds
/// the session lock. Launchers use this to branch into their
/// second-instance handoff instead of failing outright.
pub fn is_lock_conflict(err: &eyre::Report) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains(LOCK_CONFLICT_MARKER))
}

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
                eyre!(
                    "{LOCK_CONFLICT_MARKER}, lock held at {}: {err}",
                    path.display()
                )
            } else {
                eyre!(
                    "{LOCK_CONFLICT_MARKER} ({}), lock held at {}: {err}",
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

    /// Append `data_dir=` and optionally `library_name=` lines to the
    /// lock file. Called after the daemon resolves its configuration
    /// so other tools can identify which library this session serves
    /// without paying for an RPC.
    ///
    /// `library_name` is `None` when the data root was selected
    /// directly (`--data-dir` / `BOOKRACK_DATA_DIR`) and so has no
    /// registry handle.
    pub fn record_library_root(
        &mut self,
        data_dir: &Path,
        library_name: Option<&str>,
    ) -> Result<()> {
        writeln!(self.file, "data_dir={}", data_dir.display())
            .context("append session lock data_dir line")?;
        if let Some(name) = library_name {
            writeln!(self.file, "library_name={name}")
                .context("append session lock library_name line")?;
        }
        Ok(())
    }
}

/// Marker prefix of the error produced when the data-root lock is
/// already held by another process. [`is_root_lock_conflict`] matches
/// against it; both sides live in this crate so the text has a single
/// source.
const ROOT_LOCK_CONFLICT_MARKER: &str = "bookrack data root already in use";

/// Report whether `err` (or any of its causes) is the conflict error
/// produced by [`RootLock::acquire`] when another process holds the
/// data-root lock.
///
/// This is the only way to tell a contended root apart from a root
/// whose lock file could not be opened at all (a read-only volume, a
/// permission failure): the latter carries no marker, so a caller that
/// tolerates unwritable roots branches on this predicate.
pub fn is_root_lock_conflict(err: &eyre::Report) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains(ROOT_LOCK_CONFLICT_MARKER))
}

/// Exclusive lock over a data root, held by whichever process writes
/// it: the daemon for as long as it serves the root, an offline
/// destructive command for the length of its operation.
///
/// The OS releases the advisory flock when [`File`] drops, so a crashed
/// process leaves no stale lock — only stale content that the next
/// acquirer truncates. Intentionally not `Drop`-implemented because the
/// underlying file handle's drop is the release.
///
/// The recorded `pid=` / `role=` lines exist to name the conflicting
/// holder in an error message and nothing else. No reader may treat
/// them as a source of truth: the flock is the truth, its content is a
/// snapshot that can be stale the instant it is read.
pub struct RootLock {
    #[allow(dead_code)]
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl RootLock {
    /// Acquire the exclusive lock at [`root_lock_path`], recording
    /// `pid` and `role` for display in a competing acquirer's error.
    ///
    /// `role` names the holder in operator terms: `daemon`, or the
    /// command line of an offline writer such as `libraries remove
    /// --purge`.
    ///
    /// Two failures are distinguishable through
    /// [`is_root_lock_conflict`]: another process holds the lock (the
    /// error names its pid and role), or the lock file itself could not
    /// be opened or written.
    pub fn acquire(root: &Path, pid: u32, role: &str) -> Result<RootLock> {
        let path = root_lock_path(root);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open data root lock {}", path.display()))?;
        file.try_lock_exclusive().map_err(|err| {
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            let detail = existing.trim();
            if detail.is_empty() {
                eyre!(
                    "{ROOT_LOCK_CONFLICT_MARKER}, lock held at {}: {err}",
                    path.display()
                )
            } else {
                eyre!(
                    "{ROOT_LOCK_CONFLICT_MARKER} ({}), lock held at {}: {err}",
                    detail.replace('\n', ", "),
                    path.display()
                )
            }
        })?;
        let mut owned = file;
        owned
            .set_len(0)
            .context("truncate data root lock contents")?;
        write!(owned, "pid={pid}\nrole={role}\n").context("write data root lock contents")?;
        Ok(RootLock { file: owned, path })
    }
}

/// Report whether some process currently holds the flock on the
/// session lock at `path`, without disturbing its contents.
///
/// Probes with a non-blocking exclusive lock on a fresh handle:
/// acquiring it proves nobody holds the lock (the probe lock is
/// released when the handle drops on return); a contended attempt
/// proves a live holder. A missing file is trivially unheld. Any
/// other I/O failure is an `Err`, so callers can treat "cannot tell"
/// separately from either verdict.
///
/// The probe holds the exclusive lock for the duration of the check,
/// so a concurrent [`TtyLock::acquire`] racing into that window can
/// fail spuriously; use this only for advisory checks where that
/// trade-off is acceptable.
pub fn lock_is_held(path: &Path) -> Result<bool> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(eyre::Report::new(e).wrap_err(format!(
                "open session lock at {} to probe its flock",
                path.display()
            )));
        }
    };
    match file.try_lock_exclusive() {
        Ok(()) => Ok(false),
        Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(true),
        Err(e) => Err(eyre::Report::new(e)
            .wrap_err(format!("probe flock on session lock at {}", path.display()))),
    }
}

/// Snapshot of a session lock file's contents. Returned by
/// [`peek_lock`] for callers that want to discover a live session
/// (its pid, MCP listener label, control-plane socket path, and
/// served library) without taking the lock themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    pub pid: u32,
    pub mcp: String,
    pub control_sock: Option<PathBuf>,
    /// Resolved data-root path the daemon serves. Recorded by
    /// [`TtyLock::record_library_root`] once the daemon's
    /// configuration resolution completes; `None` on lock files
    /// written by daemons that crashed before that step or by an
    /// older daemon that predates the identity fields.
    pub data_dir: Option<PathBuf>,
    /// Registry name of the served library, when one was selected by
    /// name. `None` when the data root was selected directly (no
    /// registry handle) or when the lock predates the identity fields.
    pub library_name: Option<String>,
}

/// Read the session lock at `path` without acquiring it.
///
/// Returns `Ok(None)` when the file does not exist. Returns `Err`
/// when the file cannot be read, or when its contents are missing
/// the required `pid=` / `mcp=` lines or carry a `pid` value that
/// is not a `u32`. The `control_sock=`, `data_dir=`, and
/// `library_name=` lines are all optional: a lock file written by a
/// daemon that crashed mid-startup, or one written by a binary that
/// predates these fields, parses cleanly with the corresponding
/// `Option` left at `None`.
pub fn peek_lock(path: &Path) -> Result<Option<LockInfo>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(
                eyre::Report::new(e).wrap_err(format!("read session lock at {}", path.display()))
            );
        }
    };
    parse_lock(&raw, path).map(Some)
}

fn parse_lock(raw: &str, source: &Path) -> Result<LockInfo> {
    let mut pid: Option<u32> = None;
    let mut mcp: Option<String> = None;
    let mut control_sock: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut library_name: Option<String> = None;
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
        } else if let Some(value) = line.strip_prefix("data_dir=") {
            data_dir = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("library_name=") {
            library_name = Some(value.to_string());
        }
    }
    let pid = pid.ok_or_else(|| {
        eyre!(
            "session lock at {} missing required `pid=` line",
            source.display()
        )
    })?;
    let mcp = mcp.ok_or_else(|| {
        eyre!(
            "session lock at {} missing required `mcp=` line",
            source.display()
        )
    })?;
    Ok(LockInfo {
        pid,
        mcp,
        control_sock,
        data_dir,
        library_name,
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
    fn is_lock_conflict_matches_acquire_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let _held = TtyLock::acquire(&path, 1234, "127.0.0.1:8765", None).unwrap();

        let Err(err) = TtyLock::acquire(&path, 5678, "127.0.0.1:8765", None) else {
            panic!("second acquire must fail");
        };
        assert!(is_lock_conflict(&err));

        let unrelated = eyre!("disk full");
        assert!(!is_lock_conflict(&unrelated));
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
    fn lock_is_held_false_when_file_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        assert!(!lock_is_held(&path).unwrap());
    }

    #[test]
    fn lock_is_held_true_while_acquired_and_false_after_drop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let lock = TtyLock::acquire(&path, 77, "disabled", None).unwrap();
        assert!(lock_is_held(&path).unwrap());
        drop(lock);
        assert!(!lock_is_held(&path).unwrap());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("pid=77"),
            "leftover content must survive the probe: {content:?}"
        );
    }

    #[test]
    fn lock_is_held_false_on_leftover_file_never_locked() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        std::fs::write(&path, "pid=1\nmcp=disabled\n").unwrap();
        assert!(!lock_is_held(&path).unwrap());
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
        assert!(info.data_dir.is_none());
        assert!(info.library_name.is_none());
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
        assert!(info.data_dir.is_none());
        assert!(info.library_name.is_none());
    }

    #[test]
    fn peek_lock_parses_library_root_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bookrack.tty.lock");
        std::fs::write(
            &path,
            "pid=7\nmcp=disabled\ndata_dir=/data/main\nlibrary_name=main\n",
        )
        .unwrap();
        let info = peek_lock(&path).unwrap().unwrap();
        assert_eq!(info.data_dir.as_deref(), Some(Path::new("/data/main")));
        assert_eq!(info.library_name.as_deref(), Some("main"));
    }

    #[test]
    fn record_library_root_appends_data_dir_only_when_unnamed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let mut lock = TtyLock::acquire(&path, 9, "disabled", None).unwrap();
        let data_dir = PathBuf::from("/data/unnamed");
        lock.record_library_root(&data_dir, None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("data_dir=/data/unnamed"),
            "data_dir line missing: {content:?}"
        );
        assert!(
            !content.contains("library_name="),
            "unexpected library_name line: {content:?}"
        );
    }

    #[test]
    fn record_library_root_appends_both_lines_when_named() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(tty_lock_name());
        let mut lock = TtyLock::acquire(&path, 10, "disabled", None).unwrap();
        let data_dir = PathBuf::from("/data/main");
        lock.record_library_root(&data_dir, Some("main")).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("data_dir=/data/main"));
        assert!(content.contains("library_name=main"));
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

    #[test]
    fn root_lock_path_joins_the_fixed_name() {
        let root = Path::new("/data/main");
        assert_eq!(root_lock_path(root), root.join(".bookrack.lock"));
    }

    #[test]
    fn root_lock_blocks_a_second_acquirer_until_dropped() {
        let dir = tempdir().unwrap();
        let lock1 = RootLock::acquire(dir.path(), 1234, "daemon").unwrap();

        let second = RootLock::acquire(dir.path(), 5678, "daemon");
        assert!(second.is_err(), "expected second acquire to fail");

        drop(lock1);
        let _lock2 = RootLock::acquire(dir.path(), 9999, "daemon")
            .expect("re-acquire after drop must succeed");
    }

    #[test]
    fn root_lock_conflict_message_surfaces_pid_and_role() {
        let dir = tempdir().unwrap();
        let _held = RootLock::acquire(dir.path(), 7777, "daemon").unwrap();
        let err = match RootLock::acquire(dir.path(), 8888, "libraries remove --purge") {
            Ok(_) => panic!("expected lock conflict"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("7777"), "holder pid not in error: {msg}");
        assert!(
            msg.contains("role=daemon"),
            "holder role not in error: {msg}"
        );
        assert!(msg.contains("already in use"), "marker missing: {msg}");
    }

    #[test]
    fn root_lock_truncates_stale_content_on_acquire() {
        let dir = tempdir().unwrap();
        std::fs::write(
            root_lock_path(dir.path()),
            "pid=stale\nrole=stale\nextra-line\n",
        )
        .unwrap();
        let _lock = RootLock::acquire(dir.path(), 4242, "daemon").unwrap();
        let content = std::fs::read_to_string(root_lock_path(dir.path())).unwrap();
        assert!(
            content.contains("pid=4242"),
            "fresh pid missing: {content:?}"
        );
        assert!(
            content.contains("role=daemon"),
            "fresh role missing: {content:?}"
        );
        assert!(
            !content.contains("stale"),
            "stale content not truncated: {content:?}"
        );
    }

    #[test]
    fn is_root_lock_conflict_matches_acquire_error() {
        let dir = tempdir().unwrap();
        let _held = RootLock::acquire(dir.path(), 1234, "daemon").unwrap();

        let Err(err) = RootLock::acquire(dir.path(), 5678, "daemon") else {
            panic!("second acquire must fail");
        };
        assert!(is_root_lock_conflict(&err));

        let unrelated = eyre!("disk full");
        assert!(!is_root_lock_conflict(&unrelated));
    }

    #[cfg(unix)]
    #[test]
    fn root_lock_open_failure_is_not_a_conflict() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path().join("read-only-root");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).unwrap();

        let err = match RootLock::acquire(&root, 1, "daemon") {
            Ok(_) => panic!("expected acquire to fail on an unwritable root"),
            Err(e) => e,
        };
        assert!(
            !is_root_lock_conflict(&err),
            "an unopenable lock file must not read as a conflict: {err}"
        );

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
