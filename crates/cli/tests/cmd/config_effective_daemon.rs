// SPDX-License-Identifier: Apache-2.0

//! `config effective` beside a running daemon: the daemon's own values
//! are what is in use, this shell's are what a restart would resolve,
//! and the report keeps the two apart.
//!
//! The real daemons embed through the in-process [`EmbedStub`]; the
//! scripted ones answer from a table.

#![cfg(unix)]

use std::process::Output;
use std::time::Duration;

use bookrack_test_support::{DaemonProcess, EmbedStub, Sandbox, bookrack_cmd};
use serde_json::{Value, json};
use tokio::process::Command;

use super::version_skew::{Reply, StandIn, method_not_found};

/// A registry naming two libraries, `alpha` the default.
fn two_library_world() -> Sandbox {
    let sandbox = Sandbox::new();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    sandbox
}

/// A daemon over the registry, started with `extra` in its environment.
async fn daemon(sandbox: &Sandbox, extra: &[(&str, &str)]) -> DaemonProcess {
    let mut spawn = bookrack_cmd!(sandbox)
        .without_data_dir()
        .ollama_url(EmbedStub::shared().url());
    for (name, value) in extra {
        spawn = spawn.extra_env(name, value);
    }
    let mut cmd = Command::from(spawn.build());
    cmd.args(["--library", "alpha", "run"]);
    DaemonProcess::spawn(cmd)
        .expect("spawn bookrack run")
        .wait_ready(&sandbox.tty_lock_path(), Duration::from_secs(20))
        .await
        .expect("bookrack run did not become ready")
}

/// `bookrack <args>` in the sandbox, with the stub endpoint a daemon
/// there would also be given, so the endpoint row is not a difference.
async fn bookrack(sandbox: &Sandbox, args: &[&str]) -> Output {
    Command::from(
        bookrack_cmd!(sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::shared().url())
            .build(),
    )
    .args(args)
    .output()
    .await
    .expect("run bookrack")
}

fn json_of(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn row<'a>(rows: &'a Value, key: &str) -> &'a Value {
    rows.as_array()
        .and_then(|rows| rows.iter().find(|row| row["key"] == key))
        .unwrap_or_else(|| panic!("no {key} row in {rows}"))
}

/// The cells of one rendered table line: the frame stripped, the
/// columns split where two or more spaces separate them.
fn cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('\u{2502}')
        .split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .map(str::to_string)
        .collect()
}

/// The cells of the table row whose first cell is `key`.
fn table_row(text: &str, key: &str) -> Vec<String> {
    text.lines()
        .map(cells)
        .find(|cells| cells.first().is_some_and(|first| first == key))
        .unwrap_or_else(|| panic!("no {key} row:\n{text}"))
}

/// The keys of the table rows marked as this shell's own.
fn shell_rows(text: &str) -> Vec<String> {
    let mut keys: Vec<String> = text
        .lines()
        .filter(|line| line.contains("(this shell)"))
        .filter_map(|line| cells(line).into_iter().next())
        .collect();
    keys.sort();
    keys
}

async fn stop(daemon: DaemonProcess) {
    daemon
        .stop(Duration::from_secs(5))
        .await
        .expect("daemon stops on SIGTERM");
}

#[tokio::test]
async fn config_effective_reports_the_daemon_value_and_what_a_restart_would_give() {
    let sandbox = two_library_world();
    let daemon = daemon(&sandbox, &[("BOOKRACK_EMBED_BATCH_MAX_CHUNKS", "7")]).await;

    let out = bookrack(
        &sandbox,
        &["--library", "alpha", "--json", "config", "effective"],
    )
    .await;
    assert_eq!(out.status.code(), Some(0));
    let report = json_of(&out);
    // `rows` stay this shell's; the daemon's sit beside them.
    assert_eq!(
        row(&report["rows"], "embed.batch_max_chunks")["value"],
        "64"
    );
    assert_eq!(report["daemon"]["reachable"], true);
    assert_eq!(report["daemon"]["snapshot"], true);
    assert_eq!(
        row(&report["daemon"]["rows"], "embed.batch_max_chunks")["value"],
        "7"
    );
    assert_eq!(
        report["daemon"]["differences"],
        json!([{ "key": "embed.batch_max_chunks", "daemon": "7", "shell": "64" }]),
        "{report}"
    );
    assert!(report["daemon"]["identity"]["pid"].is_u64(), "{report}");

    let human = bookrack(&sandbox, &["--library", "alpha", "config", "effective"]).await;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("values in use by the running daemon (pid "),
        "the table does not say whose values it shows:\n{text}"
    );
    let cells = table_row(&text, "embed.batch_max_chunks");
    assert_eq!(
        cells.get(1).map(String::as_str),
        Some("7"),
        "the table shows this shell's value where the daemon's is in use: {cells:?}"
    );
    assert!(
        text.contains(
            "embed.batch_max_chunks  from this shell a restart would resolve 64 (default, built-in)"
        ),
        "the row does not say what a restart would give:\n{text}"
    );

    stop(daemon).await;
}

#[tokio::test]
async fn a_row_the_snapshot_lacks_is_shown_from_this_shell_and_marked() {
    let sandbox = two_library_world();
    let stranger = sandbox.data_root("stranger-root");
    let daemon = daemon(&sandbox, &[]).await;

    // A library the daemon has mounted: only the caller's own knob is
    // this shell's.
    let mounted = bookrack(&sandbox, &["--library", "beta", "config", "effective"]).await;
    let text = String::from_utf8_lossy(&mounted.stdout);
    assert_eq!(shell_rows(&text), ["confirm.timeout_secs"], "{text}");

    // A root the daemon does not serve: its library rows are this
    // shell's too.
    let unserved = bookrack(
        &sandbox,
        &[
            "--data-dir",
            &stranger.display().to_string(),
            "config",
            "effective",
        ],
    )
    .await;
    let text = String::from_utf8_lossy(&unserved.stdout);
    assert_eq!(
        shell_rows(&text),
        [
            "backup_dir",
            "confirm.timeout_secs",
            "data_dir",
            "embed.model",
            "search.top_k",
            "search.weak_threshold",
        ],
        "{text}"
    );

    stop(daemon).await;
}

#[tokio::test]
async fn config_effective_without_a_daemon_says_so_and_keeps_the_rows() {
    let sandbox = two_library_world();

    let out = bookrack(
        &sandbox,
        &["--library", "alpha", "--json", "config", "effective"],
    )
    .await;
    assert_eq!(out.status.code(), Some(0));
    let report = json_of(&out);
    assert_eq!(report["daemon"], json!({ "reachable": false }), "{report}");
    assert_eq!(
        row(&report["rows"], "embed.batch_max_chunks")["value"],
        "64"
    );

    let human = bookrack(&sandbox, &["--library", "alpha", "config", "effective"]).await;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("no daemon is running"), "{text}");
    assert!(!text.contains("(this shell)"), "{text}");
    assert!(!text.contains("Runtime state of the daemon"), "{text}");
}

#[tokio::test]
async fn the_runtime_section_shows_the_mount_set_then_and_now() {
    let sandbox = two_library_world();
    let daemon = daemon(&sandbox, &[]).await;
    let unmount = bookrack(&sandbox, &["libraries", "unmount", "beta"]).await;
    assert!(
        unmount.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&unmount.stderr)
    );

    let out = bookrack(
        &sandbox,
        &["--library", "alpha", "--json", "config", "effective"],
    )
    .await;
    let report = json_of(&out);
    assert_eq!(
        report["daemon"]["mounted_at_start"],
        json!(["alpha", "beta"])
    );
    assert_eq!(report["daemon"]["mounted_now"], json!(["alpha"]));
    assert_eq!(report["daemon"]["queue_paused"], false);

    let human = bookrack(&sandbox, &["--library", "alpha", "config", "effective"]).await;
    let text = String::from_utf8_lossy(&human.stdout);
    assert_eq!(
        table_row(&text, "mounted libraries"),
        ["mounted libraries", "alpha, beta", "alpha"],
        "{text}"
    );

    stop(daemon).await;
}

/// A scripted `daemon.config` reply: a daemon serving no library, with
/// one process row.
fn scripted_snapshot() -> Value {
    json!({
        "identity": {
            "instance_id": "018f0000-0000-7000-8000-000000000000",
            "version": env!("CARGO_PKG_VERSION"),
            "pid": 4242,
            "runtime_dir": "/run",
            "control_sock": "/run/control.sock",
            "started_at": "2026-01-01T00:00:00Z",
        },
        "dotenv_path": null,
        "rows": [{
            "key": "embed.batch_max_chunks",
            "value": "7",
            "layer": "environment",
            "site": "BOOKRACK_EMBED_BATCH_MAX_CHUNKS",
            "shadowed": [],
            "chain": [],
            "reach": "process",
            "read_at": "before_resolution",
        }],
        "retired_variables": [],
        "unclaimed_variables": null,
        "libraries": [],
        "mounted_at_start": [],
        "queue_paused": false,
    })
}

/// The `--library` flag chooses what this shell resolves. The probe
/// for the daemon's configuration carries no selection, so the flag
/// neither reaches the daemon nor gets the command refused.
#[tokio::test]
async fn config_effective_under_a_running_daemon_resolves_the_library_the_flag_names() {
    let stand_in = StandIn::start(
        env!("CARGO_PKG_VERSION"),
        &[("daemon.config", Reply::Result(scripted_snapshot()))],
        method_not_found(),
    );
    let beta = stand_in.sandbox.data_root("beta-root");
    std::fs::write(beta.join("config.toml"), "[search]\ntop_k = 3\n").expect("write config");
    stand_in
        .sandbox
        .write_registry_entries(None, &[("beta", beta.as_path())]);

    let out = stand_in
        .run(&["--library", "beta", "--json", "config", "effective"])
        .await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = json_of(&out);
    assert_eq!(
        row(&report["rows"], "search.top_k")["value"],
        "3",
        "{report}"
    );
    assert_eq!(report["daemon"]["identity"]["pid"], 4242);

    let params = stand_in.params_of("daemon.config");
    assert_eq!(params.len(), 1, "one probe: {params:?}");
    assert!(
        params[0].get("library").is_none(),
        "the probe carried the selection to the daemon: {}",
        params[0]
    );
}

/// A daemon from before `daemon.config` answers the probe with
/// method-not-found. The report is this shell's, and says why.
#[tokio::test]
async fn an_older_daemon_without_the_method_degrades_to_the_shell_report() {
    let stand_in = StandIn::start("0.14.0", &[], method_not_found());
    let root = stand_in.sandbox.data_root("main");
    let data_dir = root.display().to_string();

    let out = stand_in
        .run(&["--data-dir", &data_dir, "--json", "config", "effective"])
        .await;
    assert_eq!(out.status.code(), Some(0));
    let report = json_of(&out);
    assert_eq!(
        report["daemon"],
        json!({ "reachable": true, "snapshot": false }),
        "{report}"
    );
    assert_eq!(
        row(&report["rows"], "embed.batch_max_chunks")["value"],
        "64"
    );

    let human = stand_in
        .run(&["--data-dir", &data_dir, "config", "effective"])
        .await;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("a daemon is running and does not report its configuration"),
        "{text}"
    );
    assert!(!text.contains("(this shell)"), "{text}");
}
