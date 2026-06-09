//! Client-side primitive for the bookrack control plane.
//!
//! `discover()` reads the session lock to find the daemon's socket
//! address; `connect()` opens a stream against it; `ControlClient`
//! multiplexes JSON-RPC requests and `event` notifications over the
//! single connection. The repl client and (Phase 4) the one-shot
//! subcommand clients share this layer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, broadcast, oneshot};

/// Default broadcast capacity for the locally-fanned-out event stream.
/// The server-side broadcast that produces events runs at
/// [`bookrack_runtime::control::events::DEFAULT_EVENT_CHANNEL_CAPACITY`];
/// the local mirror buffers the same order of magnitude so a slow
/// subscriber does not stall the connection reader.
pub const DEFAULT_EVENT_CAPACITY: usize = 256;

/// A control-plane `event` notification, demultiplexed from the wire.
#[derive(Debug, Clone)]
pub struct Event {
    pub channel: String,
    pub value: Value,
    /// `true` when the server signalled that the upstream broadcast
    /// lagged this connection (see `Notification::lag` server-side).
    pub lag: bool,
}

/// Errors returned by every `ControlClient` operation.
#[derive(Debug, Error)]
pub enum ControlError {
    /// No daemon found at the resolved runtime directory: either the
    /// lock file is missing or it lacks a `control_sock` line.
    #[error("bookrack daemon not running")]
    NotRunning,
    /// Underlying I/O failure on the socket.
    #[error("control-plane transport: {0}")]
    Transport(#[from] std::io::Error),
    /// JSON-RPC error returned by the server.
    #[error("control-plane rpc error {code}: {message}")]
    Rpc {
        code: i32,
        message: String,
        data: Option<Value>,
    },
    /// Reader task exited; the connection is no longer usable.
    #[error("control-plane connection closed")]
    Closed,
    /// Malformed line from the server.
    #[error("control-plane protocol: {0}")]
    Protocol(String),
}

/// Resolved socket address for the daemon's control plane.
///
/// On Unix the address is a filesystem path to a Unix-domain socket;
/// on Windows it is a kernel-namespace pipe name.
#[derive(Debug, Clone)]
pub struct ControlSocket {
    path: PathBuf,
}

impl ControlSocket {
    /// Construct a [`ControlSocket`] from a raw address. Useful when
    /// the caller already knows the path (e.g. an integration test
    /// that brought up its own daemon).
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The resolved address. Filesystem path on Unix, named-pipe name
    /// on Windows.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read the session lock at `<runtime_dir>/bookrack.tty.lock` and
/// return the address recorded by the daemon, or [`ControlError::NotRunning`]
/// when no daemon is running.
pub fn discover(runtime_dir_override: Option<&Path>) -> Result<ControlSocket, ControlError> {
    let runtime_dir = bookrack_session::resolve_runtime_dir(runtime_dir_override)
        .map_err(|err| ControlError::Protocol(format!("resolve runtime dir: {err:#}")))?;
    let lock_path = runtime_dir.join(bookrack_session::tty_lock_name());
    let raw = match std::fs::read_to_string(&lock_path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ControlError::NotRunning);
        }
        Err(err) => return Err(ControlError::Transport(err)),
    };
    let path = raw
        .lines()
        .find_map(|line| line.strip_prefix("control_sock="))
        .map(PathBuf::from)
        .ok_or(ControlError::NotRunning)?;
    Ok(ControlSocket { path })
}

/// Open a connection to the daemon and spawn the reader task that
/// demuxes responses and events.
pub async fn connect(socket: &ControlSocket) -> Result<ControlClient, ControlError> {
    let stream = open_stream(socket).await?;
    Ok(ControlClient::spawn(stream))
}

#[cfg(unix)]
async fn open_stream(socket: &ControlSocket) -> Result<tokio::net::UnixStream, ControlError> {
    match tokio::net::UnixStream::connect(socket.path()).await {
        Ok(s) => Ok(s),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(ControlError::NotRunning),
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
            Err(ControlError::NotRunning)
        }
        Err(err) => Err(ControlError::Transport(err)),
    }
}

#[cfg(windows)]
async fn open_stream(
    socket: &ControlSocket,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, ControlError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let name = socket
        .path()
        .to_str()
        .ok_or_else(|| ControlError::Protocol("named pipe name is not utf-8".into()))?;
    match ClientOptions::new().open(name) {
        Ok(client) => Ok(client),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(ControlError::NotRunning),
        Err(err) => Err(ControlError::Transport(err)),
    }
}

type PendingMap = Mutex<HashMap<u64, oneshot::Sender<Result<Value, ControlError>>>>;

struct Inner<W> {
    writer: Mutex<W>,
    pending: Arc<PendingMap>,
    event_tx: broadcast::Sender<Event>,
    next_id: AtomicU64,
    subscribed: AtomicBool,
    closed: Arc<AtomicBool>,
}

/// Multiplexed JSON-RPC client over one daemon connection.
///
/// Holds the writer half plus a registry of pending request ids.
/// The reader half runs on a background task spawned at
/// [`connect`] time; it demuxes responses (resolves the matching
/// `oneshot`) and notifications (forwards to the broadcast).
pub struct ControlClient {
    inner: Arc<Inner<Box<dyn AsyncWriter>>>,
}

trait AsyncWriter: tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncWrite + Unpin + Send> AsyncWriter for T {}

impl ControlClient {
    fn spawn<S>(stream: S) -> Self
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (read_half, write_half) = tokio::io::split(stream);
        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, _event_rx) = broadcast::channel(DEFAULT_EVENT_CAPACITY);
        let closed = Arc::new(AtomicBool::new(false));

        let inner = Arc::new(Inner {
            writer: Mutex::new(Box::new(write_half) as Box<dyn AsyncWriter>),
            pending: pending.clone(),
            event_tx: event_tx.clone(),
            next_id: AtomicU64::new(1),
            subscribed: AtomicBool::new(false),
            closed: closed.clone(),
        });

        tokio::spawn(reader_loop(read_half, pending, event_tx, closed));

        Self { inner }
    }

    /// Send a `method`/`params` JSON-RPC request and await the response.
    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value, ControlError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(ControlError::Closed);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            pending.insert(id, tx);
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let bytes = serde_json::to_vec(&frame)
            .map_err(|err| ControlError::Protocol(format!("encode request: {err}")))?;
        if let Err(err) = self.write_frame(&bytes).await {
            let mut pending = self.inner.pending.lock().await;
            pending.remove(&id);
            return Err(err);
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(ControlError::Closed),
        }
    }

    /// Convenience over [`call_raw`] that deserialises the result.
    pub async fn call<R>(&self, method: &str, params: Value) -> Result<R, ControlError>
    where
        R: for<'de> Deserialize<'de>,
    {
        let value = self.call_raw(method, params).await?;
        serde_json::from_value(value)
            .map_err(|err| ControlError::Protocol(format!("decode response: {err}")))
    }

    /// Begin streaming `event` notifications. Subsequent calls reuse
    /// the same underlying subscription (the server only needs to be
    /// told once); each receiver sees the same events from this point
    /// forward.
    pub async fn subscribe(&self) -> Result<broadcast::Receiver<Event>, ControlError> {
        if !self.inner.subscribed.swap(true, Ordering::SeqCst)
            && let Err(err) = self
                .call_raw("events.subscribe", json!({"channels": ["*"]}))
                .await
        {
            self.inner.subscribed.store(false, Ordering::SeqCst);
            return Err(err);
        }
        Ok(self.inner.event_tx.subscribe())
    }

    /// Request a graceful daemon shutdown. The server tears down the
    /// listener; subsequent calls fail with [`ControlError::Closed`].
    pub async fn shutdown(&self) -> Result<(), ControlError> {
        let _ = self.call_raw("daemon.shutdown", Value::Null).await?;
        Ok(())
    }

    async fn write_frame(&self, bytes: &[u8]) -> Result<(), ControlError> {
        let mut writer = self.inner.writer.lock().await;
        writer.write_all(bytes).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }
}

async fn reader_loop<R>(
    read_half: R,
    pending: Arc<PendingMap>,
    event_tx: broadcast::Sender<Event>,
    closed: Arc<AtomicBool>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(read_half).lines();
    loop {
        match reader.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match parse_frame(&line) {
                Ok(Frame::Response { id, value }) => {
                    let mut map = pending.lock().await;
                    if let Some(slot) = map.remove(&id) {
                        let _ = slot.send(value);
                    }
                }
                Ok(Frame::Event(event)) => {
                    let _ = event_tx.send(event);
                }
                Err(err) => {
                    tracing::debug!(error = %err, line = %line, "control-client decode failure");
                }
            },
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(error = %err, "control-client reader I/O failure");
                break;
            }
        }
    }
    closed.store(true, Ordering::SeqCst);
    let mut map = pending.lock().await;
    for (_, slot) in map.drain() {
        let _ = slot.send(Err(ControlError::Closed));
    }
}

enum Frame {
    Response {
        id: u64,
        value: Result<Value, ControlError>,
    },
    Event(Event),
}

fn parse_frame(line: &str) -> anyhow::Result<Frame> {
    let parsed: Value = serde_json::from_str(line).context("decode frame")?;
    if parsed
        .get("method")
        .and_then(|m| m.as_str())
        .is_some_and(|m| m == "event")
    {
        let params = parsed
            .get("params")
            .ok_or_else(|| anyhow!("event missing params"))?;
        let channel = params
            .get("channel")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow!("event missing channel"))?
            .to_string();
        let value = params.get("value").cloned().unwrap_or(Value::Null);
        let lag = params.get("lag").and_then(|l| l.as_bool()).unwrap_or(false);
        return Ok(Frame::Event(Event {
            channel,
            value,
            lag,
        }));
    }
    let id = parsed
        .get("id")
        .and_then(|i| i.as_u64())
        .ok_or_else(|| anyhow!("response missing numeric id"))?;
    if let Some(err) = parsed.get("error").and_then(|e| e.as_object()) {
        let code = err
            .get("code")
            .and_then(|c| c.as_i64())
            .ok_or_else(|| anyhow!("rpc error missing code"))? as i32;
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let data = err.get("data").cloned();
        return Ok(Frame::Response {
            id,
            value: Err(ControlError::Rpc {
                code,
                message,
                data,
            }),
        });
    }
    let result = parsed.get("result").cloned().unwrap_or(Value::Null);
    Ok(Frame::Response {
        id,
        value: Ok(result),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_success() {
        let line = r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#;
        match parse_frame(line).unwrap() {
            Frame::Response {
                id,
                value: Ok(value),
            } => {
                assert_eq!(id, 7);
                assert_eq!(value, json!({"ok": true}));
            }
            _ => panic!("expected success response"),
        }
    }

    #[test]
    fn parse_response_error() {
        let line = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"no"}}"#;
        match parse_frame(line).unwrap() {
            Frame::Response {
                id,
                value: Err(ControlError::Rpc { code, message, .. }),
            } => {
                assert_eq!(id, 3);
                assert_eq!(code, -32601);
                assert_eq!(message, "no");
            }
            _ => panic!("expected rpc error"),
        }
    }

    #[test]
    fn parse_event_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"event","params":{"channel":"daemon.state","value":{"flag":"idle"}}}"#;
        match parse_frame(line).unwrap() {
            Frame::Event(event) => {
                assert_eq!(event.channel, "daemon.state");
                assert!(!event.lag);
                assert_eq!(event.value, json!({"flag": "idle"}));
            }
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn parse_event_lag() {
        let line = r#"{"jsonrpc":"2.0","method":"event","params":{"channel":"daemon.state","value":null,"lag":true}}"#;
        match parse_frame(line).unwrap() {
            Frame::Event(event) => {
                assert!(event.lag);
                assert_eq!(event.channel, "daemon.state");
            }
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn discover_missing_returns_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let result = discover(Some(dir.path()));
        assert!(matches!(result, Err(ControlError::NotRunning)));
    }

    #[test]
    fn discover_without_control_sock_returns_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join(bookrack_session::tty_lock_name());
        std::fs::write(&lock, "pid=42\nmcp=127.0.0.1:1\n").unwrap();
        let result = discover(Some(dir.path()));
        assert!(matches!(result, Err(ControlError::NotRunning)));
    }

    #[test]
    fn discover_reads_control_sock_line() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join(bookrack_session::tty_lock_name());
        std::fs::write(&lock, "pid=42\nmcp=127.0.0.1:1\ncontrol_sock=/tmp/x.sock\n").unwrap();
        let socket = discover(Some(dir.path())).unwrap();
        assert_eq!(socket.path(), Path::new("/tmp/x.sock"));
    }
}
