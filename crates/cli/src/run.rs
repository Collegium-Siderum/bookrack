// SPDX-License-Identifier: Apache-2.0

//! `bookrack run` — the daemon-REPL process entry point.
//!
//! One [`run_daemon`] call brings up the session-scoped process: it
//! acquires the machine-wide [`TtyLock`], opens the [`LibraryRegistry`]
//! that every later subsystem routes through, mounts the MCP listener
//! as an in-process task, and joins a coordinated shutdown channel
//! that signal handlers, the REPL (later phase), and the queue worker
//! (later phase) all write to.
//!
//! This phase ships the daemon skeleton without the REPL: the
//! foreground task is an `await` on the shutdown channel, so the
//! process stays alive until one of the signal handlers (or a future
//! REPL `exit`) flips it. The structure is the final one — only the
//! foreground task gets swapped for a real reedline loop later.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use bookrack_catalog::Catalog;
use bookrack_config::{
    Config, EmbedConfig, LibrarySelection, LogConfig, McpConfig, ResolutionSource, SearchConfig,
};
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Caller, Ops};
use bookrack_query::Library;
use fs2::FileExt;
use tokio::sync::broadcast;

/// Environment variable naming the session runtime directory (lock
/// file, REPL history). Optional; the default is platform-conventional.
const RUNTIME_DIR_ENV: &str = "BOOKRACK_RUNTIME_DIR";

/// Lock file held for the lifetime of one daemon session. Lives in the
/// runtime directory; its inode is reused across runs, but the lock
/// itself is released by the OS when the underlying [`File`] handle
/// drops, so a crash leaves no stale lock — only stale content (pid,
/// MCP address) that the next session overwrites.
const TTY_LOCK_NAME: &str = "bookrack.tty.lock";

/// CLI inputs for [`run_daemon`]. Parsed from the `Run` clap variant.
pub struct RunOpts {
    /// Library selection forwarded to [`Config::resolve`].
    pub selection: LibrarySelection,
    /// Override the MCP listener address; falls back to [`McpConfig::from_env`].
    pub mcp_addr: Option<SocketAddr>,
    /// Skip binding the MCP listener. The daemon still acquires the
    /// tty lock and opens the registry; useful for development and for
    /// running the daemon on a host where another tool already owns
    /// the MCP port.
    pub no_mcp: bool,
    /// Override the runtime directory. Falls back to [`RUNTIME_DIR_ENV`]
    /// or the platform default. Primarily a test hook so suites can
    /// isolate the tty lock from the operator's session.
    pub runtime_dir: Option<PathBuf>,
}

/// Run the daemon-REPL process to completion.
///
/// Returns once the shutdown broadcast fires (signal, future REPL
/// `exit`, or an MCP listener that bailed out on its own) and every
/// spawned task has joined.
pub async fn run_daemon(opts: RunOpts) -> Result<()> {
    let runtime_dir =
        resolve_runtime_dir(opts.runtime_dir.as_deref()).context("resolve BOOKRACK_RUNTIME_DIR")?;
    std::fs::create_dir_all(&runtime_dir).with_context(|| {
        format!(
            "create runtime directory {} for the bookrack session lock",
            runtime_dir.display()
        )
    })?;

    let mcp_addr = if opts.no_mcp {
        None
    } else {
        Some(
            opts.mcp_addr
                .map(|a| a.to_string())
                .unwrap_or_else(|| McpConfig::from_env().addr),
        )
    };

    let lock_path = runtime_dir.join(TTY_LOCK_NAME);
    let mcp_label = mcp_addr.clone().unwrap_or_else(|| "disabled".to_string());
    let _tty_lock = TtyLock::acquire(&lock_path, std::process::id(), &mcp_label)?;
    tracing::info!(
        path = %lock_path.display(),
        mcp = %mcp_label,
        "bookrack session lock acquired",
    );

    let cfg = Config::resolve(&opts.selection).context("resolve configuration")?;
    let _obs_guard = bookrack_obs::init(&cfg, &LogConfig::from_env());

    let embed_cfg = EmbedConfig::from_env();
    let embedder = OllamaEmbedClient::new(
        cfg.ollama_url(),
        &embed_cfg.model,
        embed_cfg.request_timeout,
        embed_cfg.max_retries,
        embed_cfg.backoff_base,
    )
    .context("build embedding client")?;

    if cfg.catalog_db().exists() {
        Catalog::open_read_only(&cfg.catalog_db())
            .context("preflight catalog schema check failed")?;
    }

    let search_cfg = SearchConfig::from_env();
    let library = Library::open(
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        embedder,
        embed_cfg.model.clone(),
        search_cfg.top_k,
    )
    .await
    .context("open query library")?;

    let ops = Ops::with_library(
        library,
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        Caller::cli(),
    );

    let library_name = cfg.library().unwrap_or("default").to_string();
    let handle = LibraryHandle::new(&library_name, ops);
    let registry = LibraryRegistry::single(handle);
    tracing::info!(library = %library_name, "library registry warmed up");

    let info_context = LibraryInfoContext {
        data_dir: cfg.data_dir().display().to_string(),
        library_name: cfg.library().map(str::to_string),
        resolution_source: resolution_source_label(cfg.source()).to_string(),
        ollama_url: cfg.ollama_url().to_string(),
        embed_model_configured: embed_cfg.model.clone(),
    };

    let (shutdown_tx, _) = broadcast::channel::<()>(8);

    let signal_handle = tokio::spawn(signal_task(shutdown_tx.clone()));

    let mcp_handle = match mcp_addr {
        Some(addr) => {
            let registry = Arc::clone(&registry);
            let rx = shutdown_tx.subscribe();
            Some(tokio::spawn(async move {
                bookrack_mcp::serve(registry, info_context, &addr, rx).await
            }))
        }
        None => {
            tracing::info!("MCP listener disabled (--no-mcp); session running without /mcp");
            None
        }
    };

    // Phase 2 foreground task: idle until shutdown. Phase 3 replaces
    // this with the reedline REPL on a `spawn_blocking` thread.
    let mut foreground_rx = shutdown_tx.subscribe();
    let _ = foreground_rx.recv().await;
    tracing::info!("shutdown signalled, joining session tasks");

    if let Some(handle) = mcp_handle {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!(error = %err, "MCP task returned error"),
            Err(err) => tracing::warn!(error = %err, "MCP task join failed"),
        }
    }
    // Signal listener has already fired (it sent shutdown) or never
    // will (e.g. process being torn down by the REPL in phase 3);
    // either way an abort is the right cleanup.
    signal_handle.abort();

    Ok(())
}

/// Resolve the runtime directory. Precedence: explicit override, then
/// [`RUNTIME_DIR_ENV`], then platform default.
fn resolve_runtime_dir(override_path: Option<&Path>) -> Result<PathBuf> {
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

fn resolution_source_label(source: ResolutionSource) -> &'static str {
    match source {
        ResolutionSource::DataDirFlag => "--data-dir flag",
        ResolutionSource::LibraryFlag => "--library flag",
        ResolutionSource::EnvVar => "BOOKRACK_DATA_DIR env",
        ResolutionSource::PortableExeNeighbor => "portable layout",
        ResolutionSource::RegistryDefault => "registry default",
        ResolutionSource::DefaultRegistryDefault => "default registry default",
        ResolutionSource::Explicit => "explicit",
    }
}

/// Drop guard for the session's tty lock.
///
/// The OS releases the advisory flock when [`File`] drops, so a
/// crashed process leaves no stale lock — only stale content (the
/// recorded pid and MCP address) that the next acquirer overwrites.
/// Held as `_tty_lock` in [`run_daemon`] for the lifetime of the
/// session; intentionally not `Drop`-implemented because the
/// underlying file handle's drop is the release.
pub(crate) struct TtyLock {
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
    pub(crate) fn acquire(path: &Path, pid: u32, mcp_addr: &str) -> Result<TtyLock> {
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

/// Aggregate the platform's shutdown signals onto the shared broadcast.
///
/// Unix listens for SIGINT, SIGTERM, and SIGHUP — the third covers
/// the "terminal window closed" path, which is the primary way the
/// session ends today. Windows listens for Ctrl-C and the close event.
async fn signal_task(shutdown_tx: broadcast::Sender<()>) -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;
        let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
        let mut sighup = signal(SignalKind::hangup()).context("install SIGHUP handler")?;
        tokio::select! {
            _ = sigint.recv() => tracing::info!("received SIGINT"),
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sighup.recv() => tracing::info!("received SIGHUP"),
        }
    }
    #[cfg(windows)]
    {
        let mut close =
            tokio::signal::windows::ctrl_close().context("install Ctrl-Close handler")?;
        tokio::select! {
            res = tokio::signal::ctrl_c() => {
                res.context("await Ctrl-C")?;
                tracing::info!("received Ctrl-C");
            }
            _ = close.recv() => tracing::info!("received Ctrl-Close"),
        }
    }
    let _ = shutdown_tx.send(());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    #[test]
    fn tty_lock_blocks_a_second_acquirer_until_dropped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(TTY_LOCK_NAME);
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
        let path = dir.path().join(TTY_LOCK_NAME);
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
        let path = dir.path().join(TTY_LOCK_NAME);
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
