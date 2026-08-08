//! `bookrack quit` — ask the running daemon to shut down. Returns 0
//! whether or not a daemon was found; daemon-not-running prints a
//! short stderr note since the user's goal (no daemon) is already met.

use std::path::PathBuf;

use bookrack_cli::library_param;
use bookrack_control_client::ControlError;
use eyre::Result;
use serde_json::Value;

pub async fn run(runtime_dir: Option<PathBuf>) -> Result<()> {
    let socket = match bookrack_control_client::discover(runtime_dir.as_deref()) {
        Ok(socket) => socket,
        Err(ControlError::NotRunning) => {
            eprintln!("bookrack: no daemon running, nothing to stop");
            return Ok(());
        }
        Err(err) => {
            eprintln!("bookrack: resolve daemon address: {err}");
            return Ok(());
        }
    };
    let client = match bookrack_control_client::connect(&socket).await {
        Ok(client) => client,
        Err(ControlError::NotRunning) => {
            eprintln!("bookrack: no daemon running, nothing to stop");
            return Ok(());
        }
        Err(err) => {
            eyre::bail!("connect to {}: {err}", socket.path().display());
        }
    };
    // Best-effort shutdown: the daemon writes its final response,
    // then tears down the listener. A `Closed` error here is the
    // expected race.
    // Same gate as every other outgoing call, though `daemon.shutdown`
    // is process-facing and the gate is a no-op on it: the exemption
    // the source check grants this file is about the call shape, not
    // about skipping the selection rules.
    let params = library_param::apply("daemon.shutdown", Value::Null)?;
    let _ = client.call_raw("daemon.shutdown", params).await;
    Ok(())
}
