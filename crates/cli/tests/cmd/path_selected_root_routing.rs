// SPDX-License-Identifier: Apache-2.0

//! A root selected by path stays reachable by path after the daemon
//! serving it mounts a registered library beside it.
//!
//! A daemon started on a root no registry entry claims serves that root
//! under an in-memory name. Once a second library is mounted, a call
//! naming no library has two places to go and is refused, so a command
//! that selected the root with `--data-dir` or `BOOKRACK_DATA_DIR` has
//! to carry the name the daemon serves the root under — and `bookrack
//! status`, which reports on that very root, has to keep answering.
//!
//! The embedder probe on the daemon's startup path, and on the mount,
//! is answered by [`EmbedStub`], so no Ollama daemon is required.

#![cfg(unix)]

use std::time::Duration;

use bookrack_catalog::Catalog;
use bookrack_test_support::{DaemonProcess, EmbedStub, Sandbox, bookrack_cmd};
use serde_json::Value;
use tokio::process::Command;

/// Run the client with `args` and return its exit code and both streams.
async fn client(cmd: Command, args: &[&str]) -> (Option<i32>, String, String) {
    let mut cmd = cmd;
    let output = cmd.args(args).output().await.expect("run bookrack");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn json(stdout: &str) -> Value {
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {stdout}"))
}

#[tokio::test]
async fn a_path_selected_root_is_reached_by_path_beside_a_mounted_library() {
    let sandbox = Sandbox::new();
    let anonymous = sandbox.data_root("anonymous-root");
    let beta = sandbox.data_root("beta-root");
    // The registry knows `beta` alone; the daemon comes up on a root it
    // does not name. Both roots carry a catalog, so the card's counts
    // below are numbers rather than the null an absent store reports.
    for root in [&anonymous, &beta] {
        drop(Catalog::open(&root.join("catalog.db")).expect("open the catalog"));
    }
    sandbox.write_registry_entries(None, &[("beta", beta.as_path())]);
    let book = sandbox.data_root("fixtures").join("synthetic-ledger.txt");
    std::fs::write(&book, "Chapter One\n\nA synthetic ledger.\n").expect("write fixture");

    let lock_path = sandbox.tty_lock_path();
    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .data_dir(&anonymous)
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.arg("run");
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&lock_path, Duration::from_secs(20))
        .await
        .expect("bookrack run did not become ready");

    let (code, stdout, stderr) = client(
        Command::from(bookrack_cmd!(&sandbox).without_data_dir().build()),
        &["rpc", "call", "library.mount", r#"{"name":"beta"}"#],
    )
    .await;
    assert_eq!(
        code,
        Some(0),
        "mount beta beside the anonymous root: {stdout}{stderr}"
    );

    // `--data-dir` naming the anonymous root reaches it, by the name the
    // daemon serves it under.
    let (code, stdout, stderr) = client(
        Command::from(bookrack_cmd!(&sandbox).without_data_dir().build()),
        &[
            "--data-dir",
            &anonymous.display().to_string(),
            "rpc",
            "call",
            "library.info",
            "{}",
        ],
    )
    .await;
    assert_eq!(
        code,
        Some(0),
        "a path selecting the served root must route: {stderr}"
    );
    // A path-selected root has no registry name, so the answer is told
    // apart by the root it reports on.
    let info = json(&stdout);
    assert_eq!(
        info["data_dir"],
        anonymous.display().to_string(),
        "the answer must be the anonymous root's, not beta's: {stdout}"
    );

    // A submit selected the same way lands on that root, not on the
    // registry library mounted beside it.
    let (code, stdout, stderr) = client(
        Command::from(bookrack_cmd!(&sandbox).without_data_dir().build()),
        &[
            "--data-dir",
            &anonymous.display().to_string(),
            "rpc",
            "call",
            "ingest.submit",
            &format!(
                r#"{{"paths":[{}]}}"#,
                Value::String(book.display().to_string())
            ),
        ],
    )
    .await;
    assert_eq!(code, Some(0), "a submit by path must route: {stderr}");
    let submitted = json(&stdout);
    let job_id = submitted["job_ids"][0]
        .as_str()
        .unwrap_or_else(|| panic!("no job id in {stdout}"))
        .to_string();
    let (code, stdout, stderr) = client(
        Command::from(bookrack_cmd!(&sandbox).without_data_dir().build()),
        &["rpc", "call", "queue.list", "{}"],
    )
    .await;
    assert_eq!(code, Some(0), "{stderr}");
    let listing = json(&stdout);
    let row = listing["jobs"]
        .as_array()
        .and_then(|jobs| {
            jobs.iter()
                .find(|job| job["id"] == Value::String(job_id.clone()))
        })
        .unwrap_or_else(|| panic!("job {job_id} not listed: {stdout}"));
    assert_eq!(
        row["library"], "default",
        "the job must be pinned to the anonymous root's name: {row}"
    );

    // `BOOKRACK_DATA_DIR` naming the bring-up root leaves `bookrack
    // status` answering: the card is about that root.
    let (code, stdout, stderr) = client(
        Command::from(bookrack_cmd!(&sandbox).data_dir(&anonymous).build()),
        &["--json", "status"],
    )
    .await;
    assert_eq!(
        code,
        Some(0),
        "status under the bring-up root must not be refused: {stderr}"
    );
    let card = json(&stdout);
    assert_eq!(
        card["library"]["data_dir"],
        anonymous.display().to_string(),
        "the card is about the selected root: {card}"
    );
    assert!(
        card["library"]["books_ready"].is_number(),
        "the card's counts come from the selected root: {card}"
    );

    daemon
        .stop(Duration::from_secs(5))
        .await
        .expect("daemon stops on SIGTERM");
}
