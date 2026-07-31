// SPDX-License-Identifier: Apache-2.0

//! Bring-up plumbing shared by the daemon integration suite.
//!
//! Host isolation is not this module's job: every test in the suite
//! opens with `bookrack_test_support::process_env`, which owns the
//! environment the daemon reads. What lives here is the deadline the
//! suite runs under, the option builder, the join helper that keeps a
//! failed driver from wedging the foreground loop, and the control
//! socket client.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use eyre::{Result, eyre};

/// Upper bound on one test's daemon lifetime. A driver that dies
/// before `daemon.shutdown` reaches the daemon would otherwise leave
/// `run_until_shutdown` blocked on the broadcast forever; the
/// deadline turns that hang into a failure.
pub const TEST_DEADLINE: Duration = Duration::from_secs(60);

/// Headless bring-up options against explicit data and runtime roots.
pub fn build_opts(
    data_dir: PathBuf,
    runtime_dir: PathBuf,
    spawn_queue_worker: bool,
) -> RuntimeOpts {
    let mut opts = RuntimeOpts::headless(Some(data_dir), None);
    opts.no_mcp = true;
    opts.spawn_queue_worker = spawn_queue_worker;
    opts.runtime_dir = Some(runtime_dir);
    opts
}

/// Drive `run_until_shutdown` and the test's driver task to completion
/// under [`TEST_DEADLINE`]. If the driver fails before its
/// `daemon.shutdown` reaches the daemon, the broadcast is fired here
/// so the foreground loop still drains and the driver's own error is
/// what surfaces.
pub async fn join_with_deadline(
    runtime: DaemonRuntime,
    repl_handle: tokio::task::JoinHandle<Result<()>>,
    driver: tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    let shutdown_tx = runtime.shutdown_tx.clone();
    tokio::time::timeout(TEST_DEADLINE, async move {
        let run = tokio::spawn(runtime.run_until_shutdown(None, repl_handle));
        let driver_result = match driver.await {
            Ok(result) => result,
            Err(join_err) => Err(eyre!("driver task failed: {join_err}")),
        };
        // Idempotent when the driver already sent `daemon.shutdown`;
        // unwedges the foreground loop when it died before doing so.
        let _ = shutdown_tx.send(());
        let run_result = run
            .await
            .map_err(|join_err| eyre!("runtime task failed: {join_err}"))?;
        driver_result.and(run_result)
    })
    .await
    .map_err(|_| eyre!("test deadline elapsed before daemon shutdown"))?
}

// Each test binary consumes its own subset of these helpers; the
// unused remainder must not fail that binary's `-D warnings` build.
#[allow(unused_imports)]
#[cfg(unix)]
pub use socket::{Reader, Writer, await_channel, connect, recv, send};

#[cfg(unix)]
mod socket {
    use std::path::Path;
    use std::time::Duration;

    use eyre::{Result, eyre};
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
    use tokio::net::UnixStream;

    pub type Reader = Lines<BufReader<ReadHalf<UnixStream>>>;
    pub type Writer = WriteHalf<UnixStream>;

    /// Connect to the control socket, retrying inside a fixed window
    /// so the driver never races the accept loop's bring-up.
    pub async fn connect(sock: &Path) -> Result<(Reader, Writer)> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(sock).await {
                Ok(stream) => break stream,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(err) => return Err(eyre!("control socket never accepted: {err}")),
            }
        };
        let (read_half, write_half) = tokio::io::split(stream);
        Ok((BufReader::new(read_half).lines(), write_half))
    }

    pub async fn send(writer: &mut Writer, line: &str) -> Result<()> {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }

    pub async fn recv(reader: &mut Reader) -> Result<Value> {
        let line = reader
            .next_line()
            .await?
            .ok_or_else(|| eyre!("connection closed before response"))?;
        Ok(serde_json::from_str(&line)?)
    }

    /// Skip frames until a notification on `channel` arrives, bounded
    /// by `timeout`.
    pub async fn await_channel(
        reader: &mut Reader,
        channel: &str,
        timeout: Duration,
    ) -> Result<Value> {
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return Err(eyre!("timed out waiting for channel {channel}")),
                frame = reader.next_line() => {
                    let line = frame?.ok_or_else(|| eyre!("eof while awaiting {channel}"))?;
                    let v: Value = serde_json::from_str(&line)?;
                    if v.get("method").is_some()
                        && v["params"]["channel"].as_str() == Some(channel)
                    {
                        return Ok(v);
                    }
                }
            }
        }
    }
}
