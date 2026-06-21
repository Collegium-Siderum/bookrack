// SPDX-License-Identifier: Apache-2.0

//! Listener and per-connection task for the control-plane socket.
//!
//! On Unix-likes the listener is a [`tokio::net::UnixListener`] bound
//! to `<runtime_dir>/control.sock`. On Windows the listener is a
//! Tokio named-pipe server bound to `\\.\pipe\bookrack-control`; the
//! `control.sock` path field of the [`ControlSocketPath`] then carries
//! that pipe name verbatim so [`crate::DaemonRuntime`] can record it
//! in the session lock the same way.
//!
//! The accept loop subscribes to the shared shutdown broadcast and
//! exits the moment a `()` is sent: no further connections are
//! accepted; already-attached clients drain through their own copies
//! of the receiver.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use super::events::Event;
use super::jsonrpc::{INTERNAL_ERROR, Notification, Request, Response, RpcError, parse_request};
use super::methods::{DispatchOutcome, MethodContext, SNAPSHOT_CHANNELS, dispatch, snapshot_for};

/// Where the listener was bound and how the lock file should record
/// it. On Unix `path` names the on-disk socket; on Windows it carries
/// the named-pipe address (e.g. `\\.\pipe\bookrack-control`) so the
/// recording shape stays uniform.
#[derive(Debug, Clone)]
pub struct ControlSocketPath {
    pub path: PathBuf,
    pub is_filesystem_path: bool,
}

impl ControlSocketPath {
    /// Unlink the socket from the filesystem on Unix; no-op on
    /// Windows (named pipes live in the kernel object namespace).
    pub fn cleanup(&self) {
        if self.is_filesystem_path
            && let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "failed to unlink control socket",
            );
        }
    }
}

/// RAII handle owning a freshly bound [`ControlSocketPath`] until
/// [`crate::DaemonRuntime::start`] has cleared every fallible bring-up
/// step. Dropping the guard without [`Self::disarm`] unlinks the
/// socket entry, so a `?` between `bind` and the final `Ok(Self)` does
/// not leave an orphan `control.sock` on disk for outside clients to
/// attach to.
pub struct ControlSocketGuard {
    sock: Option<ControlSocketPath>,
}

impl ControlSocketGuard {
    pub fn new(sock: ControlSocketPath) -> Self {
        Self { sock: Some(sock) }
    }

    /// Borrow the bound path without releasing ownership.
    pub fn path(&self) -> &Path {
        &self
            .sock
            .as_ref()
            .expect("ControlSocketGuard already disarmed")
            .path
    }

    /// Hand the inner [`ControlSocketPath`] back to the caller and
    /// disable cleanup-on-drop.
    pub fn disarm(mut self) -> ControlSocketPath {
        self.sock
            .take()
            .expect("ControlSocketGuard already disarmed")
    }
}

impl Drop for ControlSocketGuard {
    fn drop(&mut self) {
        if let Some(sock) = self.sock.take() {
            sock.cleanup();
        }
    }
}

/// Platform-typed listener handed back from [`bind`].
pub enum BoundListener {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    NamedPipe { name: String },
}

#[cfg(unix)]
const _: () = ();

#[cfg(windows)]
const WINDOWS_PIPE_NAME: &str = r"\\.\pipe\bookrack-control";

/// Bind the control-plane listener.
///
/// On Unix `runtime_dir` hosts `control.sock`; a stale entry left by
/// a crashed predecessor is unlinked here, which is safe because the
/// caller already holds the session [`crate::TtyLock`] (see
/// `DaemonRuntime::start` step 2 — bind comes after acquire).
///
/// On Windows the runtime directory is ignored; the listener binds at
/// the fixed kernel-namespace name `\\.\pipe\bookrack-control`.
pub async fn bind(runtime_dir: &Path) -> Result<(BoundListener, ControlSocketPath)> {
    #[cfg(unix)]
    {
        let path = runtime_dir.join("control.sock");
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("remove stale control socket at {}", path.display()))?;
        }
        let listener = tokio::net::UnixListener::bind(&path)
            .with_context(|| format!("bind control socket at {}", path.display()))?;
        Ok((
            BoundListener::Unix(listener),
            ControlSocketPath {
                path,
                is_filesystem_path: true,
            },
        ))
    }
    #[cfg(windows)]
    {
        let _ = runtime_dir;
        let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
        let _server = options
            .first_pipe_instance(true)
            .create(WINDOWS_PIPE_NAME)
            .with_context(|| format!("bind control named pipe {WINDOWS_PIPE_NAME}"))?;
        Ok((
            BoundListener::NamedPipe {
                name: WINDOWS_PIPE_NAME.to_string(),
            },
            ControlSocketPath {
                path: PathBuf::from(WINDOWS_PIPE_NAME),
                is_filesystem_path: false,
            },
        ))
    }
}

/// Accept incoming control-plane connections until the shared
/// shutdown broadcast fires. Each accepted connection becomes its
/// own task; the loop itself just owns the listener.
pub async fn run_accept_loop(
    listener: BoundListener,
    ctx: MethodContext,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()> {
    match listener {
        #[cfg(unix)]
        BoundListener::Unix(listener) => loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _addr)) => {
                        let ctx = ctx.clone();
                        let shutdown = ctx.shutdown_tx.subscribe();
                        tokio::spawn(async move {
                            if let Err(err) = serve_connection(stream, ctx, shutdown).await {
                                tracing::debug!(error = %err, "control connection ended");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "control accept failed");
                    }
                }
            }
        },
        #[cfg(windows)]
        BoundListener::NamedPipe { name } => {
            let mut server =
                match tokio::net::windows::named_pipe::ServerOptions::new().create(&name) {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::warn!(error = %err, "control named pipe rebind failed");
                        return Ok(());
                    }
                };
            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => break,
                    accepted = server.connect() => match accepted {
                        Ok(()) => {
                            // Rebind a fresh server instance for the next
                            // connect. A transient bind failure here must
                            // not tear down the whole accept loop (the
                            // unix arm logs-and-continues on errors), so
                            // retry with a short backoff until success or
                            // shutdown.
                            let next = loop {
                                match tokio::net::windows::named_pipe::ServerOptions::new()
                                    .create(&name)
                                {
                                    Ok(s) => break Some(s),
                                    Err(err) => {
                                        tracing::warn!(
                                            error = %err,
                                            "control named pipe rebind failed; retrying"
                                        );
                                        tokio::select! {
                                            _ = shutdown_rx.recv() => break None,
                                            _ = tokio::time::sleep(
                                                std::time::Duration::from_millis(100),
                                            ) => {}
                                        }
                                    }
                                }
                            };
                            let Some(next) = next else { break };
                            let connected = std::mem::replace(&mut server, next);
                            let ctx = ctx.clone();
                            let shutdown = ctx.shutdown_tx.subscribe();
                            tokio::spawn(async move {
                                if let Err(err) = serve_connection(connected, ctx, shutdown).await {
                                    tracing::debug!(error = %err, "control connection ended");
                                }
                            });
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "control accept failed");
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
type ConnectionStream = tokio::net::UnixStream;
#[cfg(windows)]
type ConnectionStream = tokio::net::windows::named_pipe::NamedPipeServer;

/// Serve one connection.
///
/// Frames are line-delimited JSON; the loop reads one request, runs
/// the dispatcher, writes the response, and — if the client called
/// `events.subscribe` — folds the event broadcast into the same
/// connection task so subsequent notifications reach the same client
/// in arrival order.
async fn serve_connection(
    stream: ConnectionStream,
    ctx: MethodContext,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()> {
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();
    let mut writer = write_half;

    let mut event_rx: Option<broadcast::Receiver<Event>> = None;

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            line = reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let (response, side_effect) = handle_line(&line, &ctx).await;
                        write_response(&mut writer, response).await?;
                        match side_effect {
                            SideEffect::None => {}
                            SideEffect::SnapshotBundle => {
                                emit_snapshot_bundle(&mut writer, &ctx).await?;
                                if event_rx.is_none() {
                                    event_rx = Some(ctx.event_stream.subscribe());
                                }
                            }
                            SideEffect::Shutdown => {
                                // accept loop will tear down; drain
                                // by letting the broadcast wake us.
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::debug!(error = %err, "control line read failed");
                        break;
                    }
                }
            }
            recv = recv_event(&mut event_rx) => match recv {
                Some(Ok(event)) => {
                    let notif = Notification::event(event.channel(), event.value());
                    write_notification(&mut writer, notif).await?;
                }
                Some(Err(broadcast::error::RecvError::Lagged(_))) => {
                    // The broadcast multiplexes every channel, so a lag
                    // reports the receiver fell behind without naming
                    // which channels were missed. Fan one lag marker
                    // out per snapshot channel so clients re-sync each
                    // channel via `events.snapshot` rather than acting
                    // on a single misleading channel name.
                    for channel in SNAPSHOT_CHANNELS {
                        write_notification(&mut writer, Notification::lag(*channel)).await?;
                    }
                }
                Some(Err(broadcast::error::RecvError::Closed)) => {
                    event_rx = None;
                }
                None => {
                    // No subscription yet: nothing to do, the select
                    // arm is parked on `pending`.
                }
            }
        }
    }
    let _ = writer.shutdown().await;
    Ok(())
}

enum SideEffect {
    None,
    SnapshotBundle,
    Shutdown,
}

async fn handle_line(line: &str, ctx: &MethodContext) -> (Response, SideEffect) {
    let request = match parse_request(line) {
        Ok(r) => r,
        Err((id, err)) => {
            return (
                Response::error(id.unwrap_or(serde_json::Value::Null), err),
                SideEffect::None,
            );
        }
    };
    let id = request.id.clone().unwrap_or(serde_json::Value::Null);
    let side_effect = match request.method.as_str() {
        "events.subscribe" => SideEffect::SnapshotBundle,
        "daemon.shutdown" => SideEffect::Shutdown,
        _ => SideEffect::None,
    };
    match dispatch(&request, ctx).await {
        Ok(DispatchOutcome::Result(value)) => (Response::success(id, value), side_effect),
        Ok(DispatchOutcome::Shutdown(value)) => {
            (Response::success(id, value), SideEffect::Shutdown)
        }
        Err(err) => (Response::error(id, err), SideEffect::None),
    }
}

async fn recv_event(
    event_rx: &mut Option<broadcast::Receiver<Event>>,
) -> Option<Result<Event, broadcast::error::RecvError>> {
    match event_rx {
        Some(rx) => Some(rx.recv().await),
        None => std::future::pending().await,
    }
}

async fn emit_snapshot_bundle<W>(writer: &mut W, ctx: &MethodContext) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    for channel in SNAPSHOT_CHANNELS {
        if let Some(value) = snapshot_for(channel, ctx) {
            let notif = Notification::event(*channel, value);
            write_notification(writer, notif).await?;
        }
    }
    Ok(())
}

async fn write_response<W>(writer: &mut W, response: Response) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(&response).unwrap_or_else(|_| {
        serde_json::to_vec(&Response::error(
            serde_json::Value::Null,
            RpcError::new(INTERNAL_ERROR, "response serialisation failed"),
        ))
        .expect("error response is serialisable")
    });
    bytes.push(b'\n');
    writer.write_all(&bytes).await.context("write response")?;
    writer.flush().await.context("flush response")?;
    Ok(())
}

async fn write_notification<W>(writer: &mut W, notification: Notification) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(&notification).context("serialise notification")?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .context("write notification")?;
    writer.flush().await.context("flush notification")?;
    Ok(())
}

#[allow(dead_code)]
fn _request_compile_check(_: Request) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn guard_drop_unlinks_socket_when_not_disarmed() {
        let runtime_dir = tempfile::tempdir().expect("tempdir");
        let (_listener, sock) = bind(runtime_dir.path()).await.expect("bind");
        let path = sock.path.clone();
        assert!(path.exists(), "bind should create the socket entry");

        let guard = ControlSocketGuard::new(sock);
        drop(guard);

        assert!(
            !path.exists(),
            "ControlSocketGuard drop should unlink {}",
            path.display()
        );
    }

    #[tokio::test]
    async fn guard_disarm_preserves_socket_for_caller_owned_cleanup() {
        let runtime_dir = tempfile::tempdir().expect("tempdir");
        let (_listener, sock) = bind(runtime_dir.path()).await.expect("bind");
        let path = sock.path.clone();

        let guard = ControlSocketGuard::new(sock);
        let recovered = guard.disarm();
        assert_eq!(recovered.path, path);
        assert!(
            path.exists(),
            "disarmed guard must not unlink the socket on drop",
        );

        recovered.cleanup();
        assert!(!path.exists(), "explicit cleanup unlinks the socket");
    }
}
