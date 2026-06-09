//! `bookrack doctor` — control-plane wrapper with local fallback.
//!
//! When the daemon is running we call `doctor.gather` so the report
//! lines up with the live runtime; when it is not we fall back to the
//! in-binary `bookrack_runtime::doctor::run` so a fresh install can
//! still produce a useful health summary before any session exists.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_config::LibrarySelection;
use bookrack_control_client::ControlError;
use serde_json::Value;

pub async fn run(
    selection: &LibrarySelection,
    json: bool,
    runtime_dir: Option<PathBuf>,
) -> Result<()> {
    match bookrack_control_client::discover(runtime_dir.as_deref()) {
        Ok(socket) => match bookrack_control_client::connect(&socket).await {
            Ok(client) => {
                let value = client
                    .call_raw("doctor.gather", Value::Null)
                    .await
                    .map_err(anyhow::Error::from)?;
                bookrack_runtime::doctor::render_value(&value, json)
            }
            Err(ControlError::NotRunning) => bookrack_runtime::doctor::run(selection, json).await,
            Err(err) => {
                eprintln!("bookrack: connect to {}: {err}", socket.path().display());
                bookrack_runtime::doctor::run(selection, json).await
            }
        },
        Err(ControlError::NotRunning) => bookrack_runtime::doctor::run(selection, json).await,
        Err(err) => {
            eprintln!("bookrack: resolve daemon address: {err}");
            bookrack_runtime::doctor::run(selection, json).await
        }
    }
}
