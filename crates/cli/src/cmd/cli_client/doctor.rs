//! `bookrack doctor` — control-plane wrapper with local fallback.
//!
//! When the daemon is running we call `doctor.gather` so the report
//! lines up with the live runtime; when it is not we fall back to the
//! in-binary `bookrack_runtime::doctor::run` so a fresh install can
//! still produce a useful health summary before any session exists.

use std::path::{Path, PathBuf};

use bookrack_cli::error::BookrackCliError;
use bookrack_cli::library_param;
use bookrack_config::LibrarySelection;
use bookrack_runtime::control::{HealthProbe, SessionLockState, classify_session_lock};
use eyre::{Context, Result};
use serde_json::Value;

use super::helpers;

/// Refuse the offline repair named by `flag` unless the session lock
/// shows that no daemon can be serving the library.
///
/// The guard fails closed. Only two readings of the lock let the repair
/// through: no lock file, and lock content whose `flock` nobody holds —
/// what a crashed daemon leaves behind, and what the next `bookrack run`
/// takes over without cleanup. Every other reading is a refusal with
/// the exit code the state already has elsewhere: a lock that cannot be
/// read or parsed, or one that is held but names no control socket or
/// one its daemon does not bind, is `SessionLockUnreadable` (exit 1); a held lock whose daemon does not
/// answer is `StaleSessionLock` (exit 3); a daemon that answers is
/// serving the library, and the refusal is operator input (exit 2).
async fn refuse_repair_under_a_daemon(runtime_dir: Option<&Path>, flag: &str) -> Result<()> {
    let state = classify_session_lock(runtime_dir)
        .await
        .context("resolve BOOKRACK_RUNTIME_DIR for the repair")?;
    match state {
        SessionLockState::NotHeld { .. } => Ok(()),
        SessionLockState::Unreadable { lock_path, error } => {
            Err(BookrackCliError::SessionLockUnreadable {
                message: format!(
                    "could not read the session lock at {}: {error:#}. {flag} is an offline \
                     repair and is refused while it cannot be told whether a daemon is \
                     serving this library.",
                    lock_path.display()
                ),
            }
            .into())
        }
        SessionLockState::Held {
            lock_path,
            info,
            probe,
        } => match probe {
            HealthProbe::Healthy { .. } => Err(BookrackCliError::LocalUserError {
                message: format!(
                    "a daemon is serving this library; {flag} is an offline repair. Stop the \
                     daemon with `bookrack quit` and re-run."
                ),
            }
            .into()),
            HealthProbe::Stale => Err(BookrackCliError::StaleSessionLock {
                path: lock_path,
                pid: info.pid,
            }
            .into()),
            HealthProbe::Unprobeable => Err(BookrackCliError::SessionLockUnreadable {
                message: format!(
                    "the session lock at {} is held but names no control socket, so it cannot \
                     be told whether a daemon is serving this library. {flag} is an offline \
                     repair and is refused.",
                    lock_path.display()
                ),
            }
            .into()),
            HealthProbe::InvalidEndpoint(reason) => Err(BookrackCliError::invalid_lock_endpoint(
                &lock_path,
                &reason,
                Some(&format!("{flag} is an offline repair and is refused.")),
            )
            .into()),
        },
    }
}

/// Returns `true` when the doctor report (or the envelope rename
/// summary) is clean, and `false` when at least one row is FAIL. The
/// caller maps the boolean onto a process exit code so the colored
/// table the renderer already wrote is the sole human-facing signal of
/// failure.
// The parameter list mirrors the doctor subcommand's flag set
// one-to-one; a grouping struct would only restate the clap variant.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    selection: &LibrarySelection,
    json: bool,
    install_pdfium: bool,
    install_reranker: bool,
    rename_envelopes: bool,
    close_abandoned_runs: bool,
    dry_run: bool,
    runtime_dir: Option<PathBuf>,
) -> Result<bool> {
    // The catalog-writing repair is judged first, before any install
    // runs: a refusal that arrives after a download has already landed
    // would report the one and hide the other.
    if close_abandoned_runs {
        refuse_repair_under_a_daemon(runtime_dir.as_deref(), "--close-abandoned-runs").await?;
    }
    // Every repair writes into the data root, so the root's identity is
    // judged here too, before any install. A root that does not resolve
    // is left to the repair's own resolution, which reports it with the
    // repair's context.
    if (rename_envelopes || close_abandoned_runs)
        // root identity: gated; the gate for every repair below
        && let Ok(cfg) = bookrack_config::Config::resolve(selection)
    {
        bookrack_runtime::identity_check::refuse_offline_write(&cfg)?;
    }
    // The installs are host-level actions: they land in the per-user
    // managed directories regardless of whether a daemon is running,
    // so they happen before the report path forks. A running daemon
    // picks the artifacts up on its next start.
    if install_pdfium {
        bookrack_cli::setup_cmd::install_pdfium(None, json).await?;
    }
    if install_reranker {
        bookrack_cli::setup_cmd::install_reranker(json).await?;
    }
    // Envelope rename is also a host-level filesystem walk; it runs
    // before the report path forks so the operator gets a self-
    // contained result.
    if rename_envelopes {
        let report = bookrack_runtime::doctor::rename_envelopes(selection, dry_run).await?;
        bookrack_runtime::doctor::render_rename_report(&report, json);
        return Ok(!report.has_failures());
    }
    // Closing abandoned runs writes the registry, which would race the
    // daemon's exclusive write handle, and one of the daemon's own runs
    // may be in flight. It is an offline repair, guarded above; the dry
    // run is guarded too, because a plan computed against a live
    // library describes a moment that has already passed.
    if close_abandoned_runs {
        // root identity: gated; judged at the top of run, before any install
        let cfg = bookrack_config::Config::resolve(selection)?;
        let report = bookrack_runtime::open_runs::close_abandoned_runs(&cfg, dry_run);
        bookrack_runtime::open_runs::render_close_report(&report, json);
        return Ok(!report.has_failures());
    }
    match bookrack_control_client::discover(runtime_dir.as_deref()) {
        Ok(socket) => match bookrack_control_client::connect(&socket).await {
            Ok(client) => {
                // Not opened through `helpers::connect`: this call sits
                // between two fallbacks that run without a daemon, and
                // that helper reads "no daemon" as an error. The two
                // things it would have done on the way are done here:
                // the selection is settled into a name now that a
                // daemon can answer which root it serves, and the gate
                // puts that name on the call.
                helpers::settle_library_selection(&client).await?;
                let params = library_param::apply("doctor.gather", Value::Null)?;
                let value = helpers::call_rpc(&client, "doctor.gather", params)
                    .await
                    .map_err(eyre::Report::from)?;
                bookrack_runtime::doctor::render_value(&value, json)
            }
            // A daemon that did not answer is a finding, not a
            // preamble: the in-process report's reporter row states
            // which path produced it and what it found at the lock,
            // so nothing goes to stderr ahead of the report.
            Err(_) => bookrack_runtime::doctor::run(selection, json, runtime_dir.as_deref()).await,
        },
        Err(_) => bookrack_runtime::doctor::run(selection, json, runtime_dir.as_deref()).await,
    }
}
