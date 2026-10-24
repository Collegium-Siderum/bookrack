// SPDX-License-Identifier: Apache-2.0

//! `bookrack index-profile apply` executes a plan on a library named by
//! any of the three selectors: `--library`, `--data-dir`, and the
//! data-root variable.
//!
//! Executing a plan connects to the daemon and reads `status` for the
//! worker flag and the queue depth before anything is rendered. `status`
//! reports on the process and refuses a library selection, so that read
//! carries none; each action that follows names the plan's own target.
//!
//! The library holds one synthetic book ingested against [`EmbedStub`]
//! under the default model. The target is a user profile declaring the
//! stub's other advertised model at the stub's width with an exhaustive
//! scan: the model differs from the stamp, which derives a reset, and
//! the reset's re-embed builds the cold-start ANN index, so a drop
//! follows the reset to realize the scan. The model is outside the
//! registry, which the profile may name.
//!
//! An applied plan changes the library, so before each selector the
//! library is put back under the default model with no reference —
//! through the public verbs, `libraries config --unset` and `vectors
//! reset` — and each selector meets a plan with work in it, which is the
//! only kind that reaches the daemon.
//!
//! The model is outside the registry, so a write that points the
//! library at the profile notes it once: the dry run and the confirmed
//! apply each print it, an apply under `--quiet` or `--json` prints it
//! nowhere and records it as shown nowhere, and an apply after the
//! confirmed one does not print it again. Dropping the reference clears
//! what was shown.
//!
//! Every apply converges in one run: the post-apply self-check passes,
//! and an apply after it finds nothing to execute.

#![cfg(unix)]

use std::path::Path;
use std::time::{Duration, Instant};

use bookrack_test_support::{EmbedStub, Sandbox, Spawn, bookrack_cmd};
use tokio::process::Command;

use crate::common::{DaemonProcess, wait_for_lock};

const LIBRARY: &str = "main";
const PROFILE: &str = "scan-alternate-model";

/// The note a write prints for a profile naming the stub's second model,
/// which the model registry does not list.
const NOTE: &str = "is not in the model registry";

/// The per-user profile directory, beside the registry file.
const USER_PROFILE_DIR: &str = "index-profiles";

fn profile_toml() -> String {
    format!(
        "schema_version = 1\n\
         name = \"{PROFILE}\"\n\
         description = \"The stub's second model with an exhaustive scan.\"\n\
         \n\
         [embed]\n\
         backend = \"ollama\"\n\
         model = \"{model}\"\n\
         dim = {dim}\n\
         \n\
         [ann]\n\
         kind = \"brute-force\"\n\
         num_partitions = 1\n\
         num_bits = 8\n\
         nprobes = 1\n\
         refine_factor = 1\n\
         \n\
         [reranker]\n\
         kind = \"none\"\n",
        model = EmbedStub::ALTERNATE_MODEL,
        dim = EmbedStub::DIMENSION,
    )
}

/// A synthetic text long enough for the ingest pipeline to extract
/// prose, plan chunks, and embed them against the stub.
const BOOK_TEXT: &str = "\
Chapter One

The synthetic surveyor unrolled the synthetic map and began to
measure. Every road on the map led to a fictional town, and every
fictional town had a fictional population. Measuring the roads took
the surveyor most of the morning.

By noon the map was annotated. The surveyor marked the annotation
with a fictional seal and filed it under a fictional heading.

Chapter Two

On the second day the map grew a legend. The legend listed rivers
instead of roads, and the surveyor ordered them from narrow to wide.
Ordering rivers was slower than measuring roads.
";

/// Run one client command built from `spawn` and return its exit code,
/// stdout and stderr.
async fn run(spawn: Spawn, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::from(spawn.build())
        .args(args)
        .output()
        .await
        .expect("run the bookrack client");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn apply_executes_under_each_library_selector() {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root(LIBRARY);
    let registry = sandbox.write_registry_entries(Some(LIBRARY), &[(LIBRARY, root.as_path())]);
    let profiles = registry
        .parent()
        .expect("the registry has a parent")
        .join(USER_PROFILE_DIR);
    std::fs::create_dir_all(&profiles).expect("create the user profile directory");
    std::fs::write(profiles.join(format!("{PROFILE}.toml")), profile_toml())
        .expect("write the user profile");

    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.args(["--library", LIBRARY, "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd).expect("spawn bookrack run");
    assert!(
        wait_for_lock(&sandbox.tty_lock_path(), Duration::from_secs(30)).await,
        "session lock did not appear; bookrack run may have failed to start",
    );

    // The session lock lands before the library is warm, so the ingest
    // doubles as the wait: it is answered only once the daemon serves.
    let book = sandbox.path().join("synthetic-survey.txt");
    std::fs::write(&book, BOOK_TEXT).expect("write the synthetic book");
    let book_arg = book.to_str().expect("utf-8 path");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (code, _, stderr) = run(
            bookrack_cmd!(&sandbox).without_data_dir(),
            &["ingest", book_arg],
        )
        .await;
        if code == Some(0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the fixture ingest never succeeded: {stderr}",
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let root_arg = root.to_str().expect("utf-8 path");
    let cases: [(&str, Spawn, Vec<&str>); 3] = [
        (
            "--library",
            bookrack_cmd!(&sandbox).without_data_dir(),
            vec!["--library", LIBRARY],
        ),
        (
            "--data-dir",
            bookrack_cmd!(&sandbox).without_data_dir(),
            vec!["--data-dir", root_arg],
        ),
        (
            "the data-root variable",
            bookrack_cmd!(&sandbox).data_dir(&root),
            vec![],
        ),
    ];
    for (selector, spawn, mut args) in cases {
        restore_default_model(&sandbox).await;
        assert_plan_is_reset_then_drop(&sandbox, selector).await;

        args.extend(["index-profile", "apply", PROFILE, "--yes"]);
        let (code, stdout, stderr) = run(spawn, &args).await;
        assert!(
            !stderr.contains("cannot run this command against library"),
            "{selector}: the worker probe carried the selection into status: {stderr}",
        );
        assert_plan_executed(selector, &stdout, &root);
        assert_eq!(code, Some(0), "{selector}: {stderr}");
        assert!(
            stdout.contains("profile applied and verified"),
            "{selector}: the self-check must find the library converged: {stdout}",
        );
        assert_eq!(
            stdout.matches(NOTE).count(),
            1,
            "{selector}: the confirmed apply notes the unlisted model once: {stdout}"
        );
    }

    // The library already runs the profile and was told: nothing to
    // execute, and no note.
    let (code, stdout, stderr) = run(
        bookrack_cmd!(&sandbox).without_data_dir(),
        &[
            "--library",
            LIBRARY,
            "index-profile",
            "apply",
            PROFILE,
            "--yes",
        ],
    )
    .await;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        stdout.contains("nothing else to do"),
        "a converged library has no action left: {stdout}"
    );
    assert!(!stdout.contains(NOTE), "a note already shown: {stdout}");

    // A quiet or JSON apply prints no note and records none.
    for mode in ["--quiet", "--json"] {
        restore_default_model(&sandbox).await;
        let (code, stdout, stderr) = run(
            bookrack_cmd!(&sandbox).without_data_dir(),
            &[
                mode,
                "--library",
                LIBRARY,
                "index-profile",
                "apply",
                PROFILE,
                "--yes",
            ],
        )
        .await;
        assert_eq!(code, Some(0), "{mode}: {stderr}");
        assert!(!stdout.contains(NOTE), "{mode}: {stdout}");
        let (_, stdout, _) = run(
            bookrack_cmd!(&sandbox).without_data_dir(),
            &[
                "--library",
                LIBRARY,
                "index-profile",
                "apply",
                PROFILE,
                "--dry-run",
            ],
        )
        .await;
        assert!(
            stdout.contains(NOTE),
            "{mode} recorded a note it did not print: {stdout}"
        );
    }

    daemon
        .stop(Duration::from_secs(10))
        .await
        .expect("daemon stops on SIGTERM");
}

/// Put the library back under the default model with no reference, so
/// the next apply has a reset to run.
async fn restore_default_model(sandbox: &Sandbox) {
    let (code, _, stderr) = run(
        bookrack_cmd!(sandbox).without_data_dir(),
        &["libraries", "config", LIBRARY, "--unset", "index_profile"],
    )
    .await;
    assert_eq!(code, Some(0), "drop the reference: {stderr}");
    let (code, _, stderr) = run(
        bookrack_cmd!(sandbox).without_data_dir(),
        &["--library", LIBRARY, "vectors", "reset", "--yes"],
    )
    .await;
    assert_eq!(code, Some(0), "rebuild under the default model: {stderr}");
}

/// The plan each case executes: a reset on the book pipeline, then the
/// drop of the index its re-embed builds. A rebuild in it would fail on
/// a one-book library for reasons of its own, and an empty plan would
/// never reach the daemon.
async fn assert_plan_is_reset_then_drop(sandbox: &Sandbox, selector: &str) {
    let (code, stdout, stderr) = run(
        bookrack_cmd!(sandbox).without_data_dir(),
        &[
            "--library",
            LIBRARY,
            "index-profile",
            "apply",
            PROFILE,
            "--dry-run",
        ],
    )
    .await;
    assert_eq!(
        code,
        Some(0),
        "{selector}: the dry run plans offline: {stderr}"
    );
    assert!(stdout.contains("vectors reset"), "{selector}: {stdout}");
    assert!(
        stdout.contains("vectors drop"),
        "{selector}: the reset's re-embed builds the cold-start index, which a scan \
         profile plans to drop: {stdout}"
    );
    assert!(
        !stdout.contains("vectors rebuild"),
        "{selector}: a scan profile plans no rebuild: {stdout}"
    );
    assert!(
        stdout.contains("[papers]\n  skipped"),
        "{selector}: {stdout}"
    );
    assert!(
        stdout.contains(NOTE),
        "{selector}: the dry run notes the unlisted model: {stdout}"
    );
}

/// The plan rendered against the selected library, the declaration
/// written, and both actions run to completion.
fn assert_plan_executed(selector: &str, stdout: &str, root: &Path) {
    assert!(
        stdout.contains(&format!("library: {LIBRARY} ({})", root.display())),
        "{selector}: the plan must be rendered against the selected library: {stdout}",
    );
    assert!(
        stdout.contains(&format!("declared: index_profile = '{PROFILE}'")),
        "{selector}: {stdout}",
    );
    assert!(
        stdout.contains("[1/2] books reset: done"),
        "{selector}: the reset must run to completion: {stdout}",
    );
    assert!(
        stdout.contains("[2/2] books drop-index: done"),
        "{selector}: the drop must run to completion: {stdout}",
    );
    assert!(
        !stdout.contains("apply interrupted"),
        "{selector}: {stdout}"
    );
}
