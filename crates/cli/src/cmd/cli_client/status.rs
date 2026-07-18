// SPDX-License-Identifier: Apache-2.0

//! `bookrack status` — the one-screen daemon/library/queue card.
//!
//! Unlike its `cli_client` siblings, this module does not open with
//! [`helpers::connect`]: connect translates "no daemon" into
//! [`BookrackCliError::DaemonNotRunning`] (exit 2), while for a status
//! card "not running" is a legal answer, not an error. The module
//! therefore probes the session lock first — `peek_lock`,
//! `lock_is_held`, then `control::probe` — and only connects once the
//! probe reports a healthy daemon:
//!
//! - no lock, or a leftover lock nobody holds → short card, exit 0;
//! - flock held but the control plane does not answer within 2s
//!   (stale) → [`BookrackCliError::StaleSessionLock`], exit 3;
//! - flock held but the lock names no control socket (unprobeable) →
//!   short card with the recorded pid, exit 0 — the probe made no
//!   verdict that the daemon is dead, so neither does the card;
//! - healthy → one connection, three sequential RPCs
//!   (`daemon.version`, `status`, `library.info`), full card, exit 0.
//!
//! Identity rows (`library.name`, `library.data_dir`) come from the
//! `status` RPC, never from the lock file's `data_dir=` /
//! `library_name=` lines; the lock only feeds the liveness probe and
//! the pid / endpoint rows.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_cli::error::BookrackCliError;
use bookrack_cli::render::ctx;
use bookrack_cli::render::human::bytes_human;
use bookrack_cli::render::table::{KvTable, flatten_into_kv};
use bookrack_cli::render::time::uptime_from_iso;
use bookrack_runtime::control::{HealthProbe, probe};
use bookrack_session::{LockInfo, lock_is_held, peek_lock, resolve_runtime_dir, tty_lock_name};
use eyre::{Context, Result};
use serde_json::{Value, json};

use super::helpers;

pub async fn run(runtime_dir: Option<PathBuf>) -> Result<()> {
    let resolved = resolve_runtime_dir(runtime_dir.as_deref())
        .context("resolve BOOKRACK_RUNTIME_DIR for `bookrack status`")?;
    let lock_path = resolved.join(tty_lock_name());

    let Some(info) = peek_lock(&lock_path)? else {
        return not_running_card(&lock_path);
    };
    if !lock_is_held(&lock_path)? {
        // A crashed daemon leaves lock content behind but the kernel
        // released the flock; the next `bookrack run` takes over
        // without operator cleanup, so this is "not running", not
        // "stale".
        return not_running_card(&lock_path);
    }
    match probe(&info, Duration::from_secs(2)).await {
        HealthProbe::Stale => Err(BookrackCliError::StaleSessionLock { path: lock_path }.into()),
        HealthProbe::Unprobeable => unprobeable_card(&lock_path, &info),
        // A daemon that exits between the probe and the connect
        // surfaces as `DaemonNotRunning` (exit 2); no second short
        // card for that race.
        HealthProbe::Healthy(..) => full_card(runtime_dir.as_deref(), &info).await,
    }
}

async fn full_card(runtime_dir: Option<&Path>, info: &LockInfo) -> Result<()> {
    let client = helpers::connect(runtime_dir).await?;
    let version = helpers::dispatch(&client, "daemon.version", Value::Null).await?;
    let status = helpers::dispatch(&client, "status", Value::Null).await?;
    let library = helpers::dispatch(&client, "library.info", Value::Null).await?;
    let card = compose_card(info, &version, &status, &library);
    emit_card(&card, "run 'bookrack doctor' for health checks")
}

/// Assemble the full card from the lock snapshot and the three RPC
/// responses. Endpoint rows (`pid`, `mcp`, `control`) come from the
/// lock; everything else comes from the daemon.
fn compose_card(info: &LockInfo, version: &Value, status: &Value, library: &Value) -> Value {
    json!({
        "daemon": {
            "version": version.get("version").cloned().unwrap_or(Value::Null),
            "pid": info.pid,
            "uptime": version
                .get("started_at")
                .and_then(Value::as_str)
                .map(uptime_from_iso),
            "state": status.get("state").cloned().unwrap_or(Value::Null),
            "mcp": info.mcp,
            "control": info.control_sock.as_deref().map(|p| p.display().to_string()),
        },
        "library": {
            "name": status.get("library").cloned().unwrap_or(Value::Null),
            "data_dir": status.get("data_dir").cloned().unwrap_or(Value::Null),
            "chunks": library.get("current_chunks").cloned().unwrap_or(Value::Null),
            "books_ready": library.get("ready_book_count").cloned().unwrap_or(Value::Null),
            "disk": disk_total(library).map(bytes_human),
        },
        "queue": {
            "pending": status.get("queue_pending").cloned().unwrap_or(Value::Null),
            "running": status.get("queue_running").cloned().unwrap_or(Value::Null),
            "worker": worker_label(status.get("queue_worker_enabled")),
        },
    })
}

/// Sum the `library.info` disk section (catalog, corpus, vector
/// store), or `None` when no store size was readable.
fn disk_total(library: &Value) -> Option<u64> {
    let disk = library.get("disk")?;
    let mut total = None;
    for key in ["catalog_db", "corpus_db", "lancedb_dir"] {
        if let Some(bytes) = disk.get(key).and_then(Value::as_u64) {
            total = Some(total.unwrap_or(0) + bytes);
        }
    }
    total
}

fn worker_label(enabled: Option<&Value>) -> Value {
    match enabled.and_then(Value::as_bool) {
        Some(true) => Value::String("enabled".to_string()),
        Some(false) => Value::String("disabled".to_string()),
        None => Value::Null,
    }
}

/// Short card for "no daemon": no lock, or a leftover lock nobody
/// holds. Exit 0 — the question was answered.
fn not_running_card(lock_path: &Path) -> Result<()> {
    let card = json!({
        "daemon": {
            "running": false,
            "lock": lock_path.display().to_string(),
        },
    });
    emit_card(&card, "start a daemon with 'bookrack run'")
}

/// Short card for a held lock that names no control socket: the
/// daemon may well be alive, there is just no address to probe, so
/// the card reports what the lock records and exits 0.
fn unprobeable_card(lock_path: &Path, info: &LockInfo) -> Result<()> {
    let mut card = json!({
        "daemon": {
            "running": true,
            "lock": lock_path.display().to_string(),
            "pid": info.pid,
            "mcp": info.mcp,
            "control": Value::Null,
        },
    });
    if !ctx().is_json() {
        card["daemon"]["control"] =
            Value::String("(not recorded — daemon started without a control listener)".to_string());
    }
    emit_card(
        &card,
        "restart with 'bookrack run' to bring up a control listener",
    )
}

/// Output-mode gate shared by every card shape: `--json` prints the
/// combined object (a short card is still one legal JSON object),
/// `--quiet` prints nothing and lets the exit code answer, human mode
/// renders one flattened [`KvTable`] plus the hint line.
fn emit_card(card: &Value, hint: &str) -> Result<()> {
    let ctx = ctx();
    if ctx.is_json() {
        helpers::print_value(card);
        return Ok(());
    }
    if ctx.is_quiet() {
        return Ok(());
    }
    let mut table = KvTable::new();
    flatten_into_kv(&mut table, "", card);
    println!("{}", table.render());
    println!("hint: {hint}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_info(control: Option<&str>) -> LockInfo {
        LockInfo {
            pid: 4242,
            mcp: "127.0.0.1:8391".to_string(),
            control_sock: control.map(PathBuf::from),
            data_dir: None,
            library_name: None,
        }
    }

    #[test]
    fn compose_card_sections_daemon_library_and_queue() {
        let version = json!({ "version": "0.1.0", "started_at": "2026-01-01T00:00:00Z" });
        let status = json!({
            "state": "idle",
            "queue_pending": 1,
            "queue_running": 0,
            "queue_worker_enabled": true,
            "library": "main",
            "data_dir": "/data/main",
        });
        let library = json!({
            "current_chunks": 182430,
            "ready_book_count": 947,
            "disk": { "catalog_db": 1024, "corpus_db": 1024, "lancedb_dir": 2048 },
        });
        let card = compose_card(
            &lock_info(Some("/run/control.sock")),
            &version,
            &status,
            &library,
        );
        assert_eq!(card["daemon"]["version"], "0.1.0");
        assert_eq!(card["daemon"]["pid"], 4242);
        assert_eq!(card["daemon"]["state"], "idle");
        assert_eq!(card["daemon"]["mcp"], "127.0.0.1:8391");
        assert_eq!(card["daemon"]["control"], "/run/control.sock");
        assert!(card["daemon"]["uptime"].is_string());
        assert_eq!(card["library"]["name"], "main");
        assert_eq!(card["library"]["data_dir"], "/data/main");
        assert_eq!(card["library"]["chunks"], 182430);
        assert_eq!(card["library"]["books_ready"], 947);
        assert_eq!(card["library"]["disk"], "4.0 KiB");
        assert_eq!(card["queue"]["pending"], 1);
        assert_eq!(card["queue"]["running"], 0);
        assert_eq!(card["queue"]["worker"], "enabled");
    }

    #[test]
    fn compose_card_keeps_a_path_selected_root_null_named() {
        let version = json!({ "version": "0.1.0" });
        let status = json!({ "library": Value::Null, "data_dir": "/data/anon" });
        let card = compose_card(&lock_info(None), &version, &status, &json!({}));
        assert!(card["library"]["name"].is_null());
        assert_eq!(card["library"]["data_dir"], "/data/anon");
        assert!(card["daemon"]["control"].is_null());
        assert!(card["library"]["disk"].is_null());
    }

    #[test]
    fn disk_total_sums_only_readable_stores() {
        assert_eq!(
            disk_total(&json!({ "disk": { "catalog_db": 10, "lancedb_dir": 5 } })),
            Some(15)
        );
        assert_eq!(disk_total(&json!({ "disk": {} })), None);
        assert_eq!(disk_total(&json!({})), None);
    }
}
