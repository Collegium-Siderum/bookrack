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
//!
//! The count rows are about the library those identity rows name: the
//! `library.info` call carries that name rather than going out unnamed.
//! See [`library_info_params`] for why an unnamed call answers about a
//! different library than `status` reports.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bookrack_cli::error::BookrackCliError;
use bookrack_cli::render::ctx;
use bookrack_cli::render::human::bytes_human;
use bookrack_cli::render::table::{KvTable, flatten_into_kv};
use bookrack_cli::render::time::uptime_from_iso;
use bookrack_runtime::control::methods::library_key_for;
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
        HealthProbe::Stale => Err(BookrackCliError::StaleSessionLock {
            path: lock_path,
            pid: info.pid,
        }
        .into()),
        HealthProbe::Unprobeable => unprobeable_card(&lock_path, &info),
        // A daemon that exits between the probe and the connect
        // surfaces as `DaemonNotRunning` (exit 2); no second short
        // card for that race.
        HealthProbe::Healthy(..) => full_card(runtime_dir.as_deref(), &lock_path, &info).await,
    }
}

async fn full_card(runtime_dir: Option<&Path>, lock_path: &Path, info: &LockInfo) -> Result<()> {
    let client = helpers::connect(runtime_dir).await?;
    let version = helpers::dispatch(&client, "daemon.version", Value::Null).await?;
    let status = helpers::dispatch(&client, "status", Value::Null).await?;
    let library = helpers::dispatch(&client, "library.info", library_info_params(&status)).await?;
    let card = compose_card(lock_path, info, &version, &status, &library);
    let hint = card_hint(&card);
    emit_card(&card, &hint)
}

/// Parameters for the card's `library.info` call: the library `status`
/// named, under the key that method declares.
///
/// `status` reports the primary — the library the daemon came up under.
/// An unnamed `library.info` answers for the registry's default pointer
/// instead, and the two are the same library only when the daemon came
/// up under the default. Naming the library is what keeps the card's
/// counts about the library its identity rows name.
///
/// A root selected by path has no name to send. There the daemon serves
/// that root alone, so the unnamed call has nowhere else to land.
///
/// The key is read off the runtime's method registry rather than spelled
/// out here, so a card built by this client cannot disagree with the
/// handler about how the selection is named.
fn library_info_params(status: &Value) -> Value {
    let Some(name) = status.get("library").and_then(Value::as_str) else {
        return Value::Null;
    };
    let Some(key) = library_key_for("library.info") else {
        return Value::Null;
    };
    let mut params = serde_json::Map::new();
    params.insert(key.to_string(), Value::String(name.to_string()));
    Value::Object(params)
}

/// Assemble the full card from the lock snapshot and the three RPC
/// responses. Endpoint rows (`lock`, `pid`, `mcp`, `control`) come from
/// the lock; everything else comes from the daemon.
fn compose_card(
    lock_path: &Path,
    info: &LockInfo,
    version: &Value,
    status: &Value,
    library: &Value,
) -> Value {
    let mut card = json!({
        "daemon": {
            "version": version.get("version").cloned().unwrap_or(Value::Null),
            "lock": lock_path.display().to_string(),
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
    });
    // Absent on a healthy library: a heading that is always there, and
    // almost always empty, is one the eye learns to skip.
    if let Some(unreadable) = unreadable_stores(library) {
        card["library"]["unreadable"] = unreadable;
    }
    // Same rule for the served set: a single-library daemon would get a
    // list of one, which says nothing the rows above have not.
    if let Some(served) = plural_served(status) {
        if let Some(reached) = unnamed_calls_reach(&served, status) {
            card["library"]["default_library"] = Value::String(reached);
        }
        card["library"]["served"] = served;
    }
    card
}

/// The `served` set when this daemon holds more than one library, else
/// `None`.
fn plural_served(status: &Value) -> Option<Value> {
    let served = status.get("served")?.as_array()?;
    (served.len() > 1).then(|| Value::Array(served.clone()))
}

/// The library a call naming none would reach, when that is **not** the
/// library this card is about; `None` when the two agree.
///
/// The card reports the primary. An unnamed call resolves to the
/// registry default instead, so on a daemon started under a non-default
/// library the operator is reading one library's card while their next
/// unqualified write lands on another. That is worth a row of its own —
/// the `default` mark inside `served` states the same fact, but only to
/// a reader who already knows to look for the difference.
fn unnamed_calls_reach(served: &Value, status: &Value) -> Option<String> {
    let primary = status.get("library").and_then(Value::as_str);
    let default = served
        .as_array()?
        .iter()
        .find(|row| row["default"] == true)?["name"]
        .as_str()?;
    (Some(default) != primary).then(|| default.to_string())
}

/// The footer a full card ends on. A store that could not be read is
/// the one finding the card states without explaining, so it sends the
/// reader to the per-store schema check rather than to the environment
/// sweep `doctor` performs.
///
/// `verify` takes a library selection, so the hint names the library
/// the card is about: on a multi-library daemon the bare command would
/// check the registry default, which is not the library whose store the
/// card just reported as unreadable. `doctor` takes no selection, so
/// that hint stays bare.
fn card_hint(card: &Value) -> String {
    if card["library"].get("unreadable").is_none() {
        return "run 'bookrack doctor' for health checks".to_string();
    }
    let verify = match card["library"]["name"].as_str() {
        Some(name) => format!("bookrack --library {name} verify"),
        None => "bookrack verify".to_string(),
    };
    format!("a store could not be read -- run '{verify}' for the per-store schema check")
}

/// Every store `library.info` reports a read failure for, keyed by the
/// store an operator would name. `None` when all of them opened.
fn unreadable_stores(library: &Value) -> Option<Value> {
    let mut found = serde_json::Map::new();
    for (store, pointer) in [
        ("catalog", "/catalog_error"),
        ("corpus", "/corpus_error"),
        ("vectors", "/vectors_error"),
        ("papers_catalog", "/papers/catalog_error"),
        ("papers_corpus", "/papers/corpus_error"),
        ("papers_vectors", "/papers/vectors_error"),
    ] {
        if let Some(reason) = library.pointer(pointer).and_then(Value::as_str) {
            found.insert(store.to_string(), Value::String(reason.to_string()));
        }
    }
    (!found.is_empty()).then_some(Value::Object(found))
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
///
/// Carries one row the full card has no use for: which library a
/// `bookrack run` here would serve. On the full card that library is
/// already the subject of every row; here it is the only thing an
/// operator can act on.
fn not_running_card(lock_path: &Path) -> Result<()> {
    let card = json!({
        "daemon": {
            "running": false,
            "lock": lock_path.display().to_string(),
        },
        "registry": {
            "default": registry_default(bookrack_config::list_libraries()),
        },
    });
    emit_card(&card, "start a daemon with 'bookrack run'")
}

/// The `registry.default` row's three states, from one registry read.
///
/// A registry that cannot be read is reported **in the row** rather
/// than bubbled: this card's whole job is to answer "is a daemon
/// running" for a machine that may not be configured at all, and
/// failing the command over the follow-up question would take the
/// answer away with it. The card still exits 0.
///
/// * a `default` entry — its name;
/// * no registry, or a registry naming no default — `null`, rendered
///   `(none)`. Both are "nobody has been chosen", and a card that
///   distinguished them would be answering a question about files;
/// * a registry that could not be read — `{"error": "<one line>"}`,
///   rendered `(unreadable: …)`, so a machine-readable consumer can
///   tell it from a name and from `null`.
fn registry_default(
    entries: Result<Option<Vec<bookrack_config::LibraryEntry>>, bookrack_config::ConfigError>,
) -> Value {
    match entries {
        Ok(entries) => entries
            .unwrap_or_default()
            .into_iter()
            .find(|e| e.is_default)
            .map(|e| Value::String(e.name))
            .unwrap_or(Value::Null),
        Err(e) => json!({ "error": e.to_string() }),
    }
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
    flatten_into_kv(&mut table, "", &for_human(card));
    println!("{}", table.render());
    println!("hint: {hint}");
    Ok(())
}

/// The card rewritten where the table renders a value worse than a
/// sentence does. Two places qualify.
///
/// The served set: [`flatten_into_kv`] renders an array as a single
/// line of compact JSON, which for this array is a screenful of quoted
/// paths on one row. Keying each entry by its library name gives the
/// table what it renders well — one `library.served.<name>` row each.
///
/// The registry row: `null` flattens to an empty cell and the
/// unreadable state to a nested `registry.default.error` row, neither
/// of which reads as the answer it is. Both become words.
///
/// The `--json` twin keeps the shapes the daemon and the registry
/// reported, so no consumer has to parse these strings back.
fn for_human(card: &Value) -> Value {
    let mut human = card.clone();
    if let Some(served) = card.pointer("/library/served").and_then(Value::as_array) {
        human["library"]["served"] = Value::Object(served_rows(served));
    }
    match card.pointer("/registry/default") {
        Some(Value::Null) => {
            human["registry"]["default"] = Value::String("(none)".to_string());
        }
        Some(Value::Object(state)) => {
            if let Some(reason) = state.get("error").and_then(Value::as_str) {
                human["registry"]["default"] = Value::String(format!("(unreadable: {reason})"));
            }
        }
        _ => {}
    }
    human
}

/// One `<name> -> <root> (<marks>)` entry per served library.
fn served_rows(served: &[Value]) -> serde_json::Map<String, Value> {
    let mut rows = serde_json::Map::new();
    for row in served {
        let Some(name) = row["name"].as_str() else {
            continue;
        };
        let root = row["data_dir"].as_str().unwrap_or("(unknown root)");
        let mut marks: Vec<&str> = Vec::new();
        if row["default"] == true {
            marks.push("default");
        }
        if row["primary"] == true {
            marks.push("primary");
        }
        let rendered = if marks.is_empty() {
            root.to_string()
        } else {
            format!("{root} ({})", marks.join(", "))
        };
        rows.insert(name.to_string(), Value::String(rendered));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_info(control: Option<&str>) -> LockInfo {
        LockInfo {
            pid: 4242,
            mcp: "127.0.0.1:8391".to_string(),
            control_sock: control.map(PathBuf::from),
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
            Path::new("/run/bookrack.tty.lock"),
            &lock_info(Some("/run/control.sock")),
            &version,
            &status,
            &library,
        );
        assert_eq!(card["daemon"]["version"], "0.1.0");
        assert_eq!(card["daemon"]["pid"], 4242);
        // The lock path is on every card shape: the full card is the
        // only place a running daemon's lock file can be looked up.
        assert_eq!(card["daemon"]["lock"], "/run/bookrack.tty.lock");
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
    fn compose_card_surfaces_a_store_that_cannot_be_read() {
        // `library.info` reports why each store failed to open. Dropping
        // those fields left the card showing a null chunk count and no
        // reason for it -- on the one surface an operator reaches for
        // when something is wrong.
        let library = json!({
            "current_chunks": Value::Null,
            "catalog_error": "catalog.db: schema version 99 is newer than this binary",
            "papers": { "corpus_error": "papers_corpus.db: file is not a database" },
        });
        let card = compose_card(
            Path::new("/run/bookrack.tty.lock"),
            &lock_info(Some("/run/control.sock")),
            &json!({ "version": "0.1.0" }),
            &json!({ "library": "main", "data_dir": "/data/main" }),
            &library,
        );
        assert_eq!(
            card["library"]["unreadable"]["catalog"],
            "catalog.db: schema version 99 is newer than this binary",
        );
        assert_eq!(
            card["library"]["unreadable"]["papers_corpus"],
            "papers_corpus.db: file is not a database",
        );
    }

    #[test]
    fn compose_card_omits_the_unreadable_section_on_a_healthy_library() {
        // The section is absent, not empty: a card that always carries
        // an "unreadable" heading trains the eye to skip it.
        let card = compose_card(
            Path::new("/run/bookrack.tty.lock"),
            &lock_info(None),
            &json!({ "version": "0.1.0" }),
            &json!({ "library": "main", "data_dir": "/data/main" }),
            &json!({ "current_chunks": 5, "papers": { "current_chunks": 1 } }),
        );
        assert!(card["library"].get("unreadable").is_none(), "{card}");
    }

    #[test]
    fn the_footer_points_at_the_check_that_answers_what_the_card_shows() {
        let healthy = json!({ "library": { "name": "main" } });
        assert!(card_hint(&healthy).contains("doctor"));
        let broken = json!({ "library": { "unreadable": { "catalog": "boom" } } });
        assert!(
            card_hint(&broken).contains("verify"),
            "{}",
            card_hint(&broken)
        );
    }

    #[test]
    fn compose_card_keeps_a_path_selected_root_null_named() {
        let version = json!({ "version": "0.1.0" });
        let status = json!({ "library": Value::Null, "data_dir": "/data/anon" });
        let card = compose_card(
            Path::new("/run/bookrack.tty.lock"),
            &lock_info(None),
            &version,
            &status,
            &json!({}),
        );
        assert!(card["library"]["name"].is_null());
        assert_eq!(card["library"]["data_dir"], "/data/anon");
        assert!(card["daemon"]["control"].is_null());
        assert!(card["library"]["disk"].is_null());
    }

    /// The three states of the short card's registry row, and how each
    /// reads once the table has it. The unreadable one is the reason
    /// this is a row rather than a bubbled error: the card's own
    /// question — is a daemon running — was answered.
    #[test]
    fn the_registry_row_separates_a_name_from_nobody_from_unreadable() {
        use bookrack_config::{ConfigError, LibraryEntry, LibraryKind};

        let entry = |name: &str, is_default: bool| LibraryEntry {
            name: name.to_string(),
            data_dir: PathBuf::from(format!("/data/{name}")),
            is_default,
            kind: LibraryKind::Prod,
            description: None,
            index_profile: None,
            created_at: None,
            uuid: None,
        };

        assert_eq!(
            registry_default(Ok(Some(vec![entry("alpha", false), entry("beta", true)]))),
            "beta",
        );

        // No registry and a registry naming no default are the same
        // answer: nobody has been chosen.
        assert_eq!(registry_default(Ok(None)), Value::Null);
        assert_eq!(
            registry_default(Ok(Some(vec![entry("alpha", false)]))),
            Value::Null
        );

        let unreadable = registry_default(Err(ConfigError::RegistryUnreadable {
            path: PathBuf::from("/x/registry.toml"),
            source: std::io::Error::other("unexpected character"),
        }));
        assert!(
            unreadable["error"].as_str().is_some_and(|e| !e.is_empty()),
            "the state is machine-distinguishable from a name and from null: {unreadable}",
        );

        let human = for_human(&json!({ "registry": { "default": Value::Null } }));
        assert_eq!(human["registry"]["default"], "(none)");
        let human = for_human(&json!({ "registry": { "default": { "error": "boom" } } }));
        assert_eq!(human["registry"]["default"], "(unreadable: boom)");
    }

    /// A daemon serving one library gets no served list: it would
    /// repeat the identity rows above it and say nothing more.
    #[test]
    fn a_single_library_daemon_gets_no_served_list() {
        let status = json!({
            "library": "main",
            "data_dir": "/data/main",
            "served": [
                { "name": "main", "data_dir": "/data/main", "default": true, "primary": true },
            ],
        });
        let card = compose_card(
            Path::new("/run/bookrack.tty.lock"),
            &lock_info(None),
            &json!({ "version": "0.1.0" }),
            &status,
            &json!({}),
        );
        assert!(card["library"].get("served").is_none(), "{card}");
        assert!(card["library"].get("default_library").is_none(), "{card}");
    }

    /// More than one library: every one is listed, and the row saying
    /// where an unnamed call lands appears only because the daemon came
    /// up under a library that is not the default.
    #[test]
    fn a_multi_library_daemon_lists_them_and_names_where_unnamed_calls_land() {
        let status = json!({
            "library": "beta",
            "data_dir": "/data/beta",
            "served": [
                { "name": "alpha", "data_dir": "/data/alpha", "default": true, "primary": false },
                { "name": "beta", "data_dir": "/data/beta", "default": false, "primary": true },
            ],
        });
        let card = compose_card(
            Path::new("/run/bookrack.tty.lock"),
            &lock_info(None),
            &json!({ "version": "0.1.0" }),
            &status,
            &json!({}),
        );
        let served = card["library"]["served"]
            .as_array()
            .unwrap_or_else(|| panic!("served list missing: {card}"));
        assert_eq!(served.len(), 2, "{card}");
        assert_eq!(
            card["library"]["default_library"], "alpha",
            "the card is about beta while an unnamed call reaches alpha: {card}",
        );

        // The human table gets one row per library, marked; the JSON
        // twin above keeps the array.
        let human = for_human(&card);
        assert_eq!(
            human["library"]["served"]["alpha"], "/data/alpha (default)",
            "{human}",
        );
        assert_eq!(
            human["library"]["served"]["beta"], "/data/beta (primary)",
            "{human}",
        );
    }

    /// Coming up under the default is the ordinary multi-library case:
    /// the list is still there, the extra row is not.
    #[test]
    fn a_primary_that_is_the_default_needs_no_extra_row() {
        let status = json!({
            "library": "alpha",
            "data_dir": "/data/alpha",
            "served": [
                { "name": "alpha", "data_dir": "/data/alpha", "default": true, "primary": true },
                { "name": "beta", "data_dir": "/data/beta", "default": false, "primary": false },
            ],
        });
        let card = compose_card(
            Path::new("/run/bookrack.tty.lock"),
            &lock_info(None),
            &json!({ "version": "0.1.0" }),
            &status,
            &json!({}),
        );
        assert!(card["library"]["served"].is_array(), "{card}");
        assert!(card["library"].get("default_library").is_none(), "{card}");
        assert_eq!(
            for_human(&card)["library"]["served"]["alpha"],
            "/data/alpha (default, primary)",
        );
    }

    /// The store-failure footer names the library the card is about:
    /// `verify` is routed, so the bare command would check whichever
    /// library the registry defaults to.
    #[test]
    fn the_verify_hint_names_the_library_whose_store_failed() {
        let card = json!({
            "library": { "name": "beta", "unreadable": { "catalog": "boom" } },
        });
        let hint = card_hint(&card);
        assert!(hint.contains("--library beta verify"), "{hint}");

        // A path-selected root has no name to pass, and such a daemon
        // serves that root alone.
        let anonymous = json!({
            "library": { "name": Value::Null, "unreadable": { "catalog": "boom" } },
        });
        let hint = card_hint(&anonymous);
        assert!(hint.contains("'bookrack verify'"), "{hint}");
    }

    /// The name travels under the key the handler declares, not under a
    /// spelling this module chose: `library.info` selects with `name`
    /// where every other routed method uses `library`.
    #[test]
    fn library_info_is_asked_about_the_library_status_named() {
        let params = library_info_params(&json!({ "library": "beta" }));
        assert_eq!(params, json!({ "name": "beta" }));
    }

    /// A path-selected root reports a null name. Sending `{"name":
    /// null}` would be a selection the daemon has to reject, so the call
    /// goes out unnamed — that root is the only one such a daemon holds.
    #[test]
    fn a_path_selected_root_is_asked_without_a_name() {
        assert_eq!(
            library_info_params(&json!({ "library": Value::Null })),
            Value::Null
        );
        assert_eq!(library_info_params(&json!({})), Value::Null);
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
