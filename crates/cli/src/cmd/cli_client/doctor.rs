//! `bookrack doctor` — control-plane wrapper with local fallback.
//!
//! When the daemon is running we call `doctor.gather` so the report
//! lines up with the live runtime; when it is not we fall back to the
//! in-binary `bookrack_runtime::doctor::run` so a fresh install can
//! still produce a useful health summary before any session exists.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_cli::error::BookrackCliError;
use bookrack_cli::library_param;
use bookrack_config::LibrarySelection;
use bookrack_control_client::ControlError;
use bookrack_runtime::control::{HealthProbe, probe};
use bookrack_session::{lock_is_held, peek_lock, resolve_runtime_dir, tty_lock_name};
use eyre::{Context, Result};
use serde_json::Value;

/// Refuse the offline repair named by `flag` unless the session lock
/// shows that no daemon can be serving the library.
///
/// The guard fails closed. Only two readings of the lock let the repair
/// through: no lock file, and lock content whose `flock` nobody holds —
/// what a crashed daemon leaves behind, and what the next `bookrack run`
/// takes over without cleanup. Every other reading is a refusal with
/// the exit code the state already has elsewhere: a lock that cannot be
/// read or parsed, or one that is held but names no control socket, is
/// `SessionLockUnreadable` (exit 1); a held lock whose daemon does not
/// answer is `StaleSessionLock` (exit 3); a daemon that answers is
/// serving the library, and the refusal is operator input (exit 2).
async fn refuse_repair_under_a_daemon(runtime_dir: Option<&Path>, flag: &str) -> Result<()> {
    let resolved =
        resolve_runtime_dir(runtime_dir).context("resolve BOOKRACK_RUNTIME_DIR for the repair")?;
    let lock_path = resolved.join(tty_lock_name());
    let info = match peek_lock(&lock_path) {
        Ok(Some(info)) => info,
        Ok(None) => return Ok(()),
        Err(err) => {
            return Err(BookrackCliError::SessionLockUnreadable {
                message: format!(
                    "could not read the session lock at {}: {err:#}. {flag} is an offline \
                     repair and is refused while it cannot be told whether a daemon is \
                     serving this library.",
                    lock_path.display()
                ),
            }
            .into());
        }
    };
    if !lock_is_held(&lock_path)? {
        return Ok(());
    }
    match probe(&info, Duration::from_secs(2)).await {
        HealthProbe::Healthy(..) => Err(BookrackCliError::LocalUserError {
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
                "the session lock at {} is held but names no control socket, so it cannot be \
                 told whether a daemon is serving this library. {flag} is an offline repair \
                 and is refused.",
                lock_path.display()
            ),
        }
        .into()),
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
    backfill_ocr_derivation: bool,
    close_abandoned_runs: bool,
    dry_run: bool,
    runtime_dir: Option<PathBuf>,
) -> Result<bool> {
    // The two catalog-writing repairs are judged first, before any
    // install runs: a refusal that arrives after a download has
    // already landed would report the one and hide the other.
    if backfill_ocr_derivation {
        refuse_repair_under_a_daemon(runtime_dir.as_deref(), "--backfill-ocr-derivation").await?;
    }
    if close_abandoned_runs {
        refuse_repair_under_a_daemon(runtime_dir.as_deref(), "--close-abandoned-runs").await?;
    }
    // The installs are host-level actions: they land in the per-user
    // managed directories regardless of whether a daemon is running,
    // so they happen before the report path forks. A running daemon
    // picks the artifacts up on its next start.
    if install_pdfium {
        let path = bookrack_runtime::pdfium_install::install_pinned_pdfium().await?;
        if !json {
            println!("installed {}", path.display());
        }
    }
    if install_reranker {
        let outcomes = bookrack_runtime::reranker_install::install_reranker().await?;
        if !json {
            for outcome in outcomes {
                if outcome.downloaded {
                    println!("installed {}", outcome.path.display());
                } else {
                    println!("{} already installed", outcome.path.display());
                }
            }
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
    // race the daemon's exclusive write handle. It is an offline repair,
    // guarded above; the dry run is guarded too — it still opens the
    // catalog, and a plan against a library the operator is about to
    // take offline is misleading.
    if backfill_ocr_derivation {
        let report = bookrack_runtime::doctor::backfill_ocr_derivation(selection, dry_run).await?;
        bookrack_runtime::doctor::render_backfill_report(&report, json);
        return Ok(!report.has_failures());
    }
    // Closing abandoned runs writes the registry, so it carries the
    // same offline rule as the backfill: a daemon serving the library
    // holds the write handle, and one of its own runs may be in flight.
    // The dry run is guarded for the same reason the backfill's is — a
    // plan computed against a live library describes a moment that has
    // already passed.
    if close_abandoned_runs {
        let cfg = bookrack_config::Config::resolve(selection)?;
        let report = bookrack_runtime::open_runs::close_abandoned_runs(&cfg, dry_run);
        bookrack_runtime::open_runs::render_close_report(&report, json);
        return Ok(!report.has_failures());
    }
    match bookrack_control_client::discover(runtime_dir.as_deref()) {
        Ok(socket) => match bookrack_control_client::connect(&socket).await {
            Ok(client) => {
                // Not routed through `helpers::dispatch`: this call
                // sits between two fallbacks that run without a
                // daemon. It still passes the selection gate, which
                // refuses a `--library` naming a library this report
                // cannot be about.
                let params = library_param::apply("doctor.gather", Value::Null)?;
                let value = client
                    .call_raw("doctor.gather", params)
                    .await
                    .map_err(eyre::Report::from)?;
                bookrack_runtime::doctor::render_value(&value, json)
            }
            Err(ControlError::NotRunning) => {
                bookrack_runtime::doctor::run(selection, json, runtime_dir.as_deref()).await
            }
            Err(err) => {
                eprintln!("bookrack: connect to {}: {err}", socket.path().display());
                bookrack_runtime::doctor::run(selection, json, runtime_dir.as_deref()).await
            }
        },
        Err(ControlError::NotRunning) => {
            bookrack_runtime::doctor::run(selection, json, runtime_dir.as_deref()).await
        }
        Err(err) => {
            eprintln!("bookrack: resolve daemon address: {err}");
            bookrack_runtime::doctor::run(selection, json, runtime_dir.as_deref()).await
        }
    }
}
