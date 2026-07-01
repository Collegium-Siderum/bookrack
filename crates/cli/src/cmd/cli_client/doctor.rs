//! `bookrack doctor` — control-plane wrapper with local fallback.
//!
//! When the daemon is running we call `doctor.gather` so the report
//! lines up with the live runtime; when it is not we fall back to the
//! in-binary `bookrack_runtime::doctor::run` so a fresh install can
//! still produce a useful health summary before any session exists.

use std::path::{Path, PathBuf};

use bookrack_config::LibrarySelection;
use bookrack_control_client::ControlError;
use eyre::Result;
use serde_json::Value;

/// True when a daemon is reachable on the control socket for
/// `runtime_dir`. Used to keep offline repairs off a live library.
async fn daemon_is_running(runtime_dir: Option<&Path>) -> bool {
    match bookrack_control_client::discover(runtime_dir) {
        Ok(socket) => bookrack_control_client::connect(&socket).await.is_ok(),
        Err(_) => false,
    }
}

/// Returns `true` when the doctor report (or the envelope rename
/// summary) is clean, and `false` when at least one row is FAIL. The
/// caller maps the boolean onto a process exit code so the colored
/// table the renderer already wrote is the sole human-facing signal of
/// failure.
pub async fn run(
    selection: &LibrarySelection,
    json: bool,
    install_pdfium: bool,
    rename_envelopes: bool,
    backfill_ocr_derivation: bool,
    dry_run: bool,
    runtime_dir: Option<PathBuf>,
) -> Result<bool> {
    // The install is a host-level action: it lands in the per-user
    // managed directory regardless of whether a daemon is running, so
    // it happens before the report path forks. A running daemon picks
    // the library up on its next start.
    if install_pdfium {
        let path = bookrack_runtime::pdfium_install::install_pinned_pdfium().await?;
        if !json {
            println!("installed {}", path.display());
        }
    }
    // Envelope rename is also a host-level filesystem walk; it runs
    // before the report path forks so the operator gets a self-
    // contained result.
    if rename_envelopes {
        let report = bookrack_runtime::doctor::rename_envelopes(selection, dry_run).await?;
        bookrack_runtime::doctor::render_rename_report(&report, json);
        return Ok(!report.has_failures());
    }
    // The derivation backfill opens the catalog for writing, which would
    // race the daemon's exclusive write handle. It is an offline repair:
    // refuse it while a daemon is serving this library rather than
    // corrupt a live session. The dry run is refused too — it still opens
    // the catalog, and a plan against a library the operator is about to
    // take offline is misleading.
    if backfill_ocr_derivation {
        if daemon_is_running(runtime_dir.as_deref()).await {
            eyre::bail!(
                "a daemon is serving this library; --backfill-ocr-derivation is an \
                 offline repair. Stop the daemon with `bookrack quit` and re-run."
            );
        }
        let report = bookrack_runtime::doctor::backfill_ocr_derivation(selection, dry_run).await?;
        bookrack_runtime::doctor::render_backfill_report(&report, json);
        return Ok(!report.has_failures());
    }
    match bookrack_control_client::discover(runtime_dir.as_deref()) {
        Ok(socket) => match bookrack_control_client::connect(&socket).await {
            Ok(client) => {
                let value = client
                    .call_raw("doctor.gather", Value::Null)
                    .await
                    .map_err(eyre::Report::from)?;
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
