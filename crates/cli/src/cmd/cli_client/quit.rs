//! `bookrack quit` — ask the running daemon to shut down. Returns 0
//! whether or not a daemon was found; daemon-not-running prints a
//! short stderr note since the user's goal (no daemon) is already met.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_control_client::ControlError;
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
            anyhow::bail!("connect to {}: {err}", socket.path().display());
        }
    };
    // Best-effort shutdown: the daemon writes its final response,
    // then tears down the listener. A `Closed` error here is the
    // expected race.
    let _ = client.call_raw("daemon.shutdown", Value::Null).await;
    Ok(())
}
