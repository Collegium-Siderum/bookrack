// SPDX-License-Identifier: Apache-2.0

//! `bookrack show reference:<slug>/<key>` reads one entry end to end,
//! and every read by id refuses an item the library does not hold.
//!
//! `alpha` carries a two-entry reference book built offline through
//! `distill build`; `beta` has no reference store. The daemon comes up
//! under `alpha` with the embed stub answering its probe, and the
//! assertions drive the `bookrack` binary as a child:
//!
//! * the keyed id reads through the control plane, as JSON and as a
//!   card, and a headword as written is matched through its latin form
//!   with the card saying so;
//! * a key nobody wrote, a book or paper id the catalog does not hold,
//!   and a reference read on a library without a store all exit `2`
//!   with the three-part refusal and nothing on stdout, under `--json`
//!   as well;
//! * a `reference:` id without a key is refused as a whole book.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use bookrack_cli_grammar::{DistillAction, DistillBuildArgs};
use bookrack_config::LibrarySelection;
use bookrack_test_support::fixtures::{DISTILL_TINY_BOOK_TOML, DISTILL_TINY_SOURCE};
use bookrack_test_support::{DaemonProcess, EmbedStub, Sandbox, bookrack_cmd};
use serde_json::Value;
use tokio::process::Command;

/// Build the `tiny` reference book into `root` through the offline
/// `distill build` entry point, as an operator would.
async fn build_tiny_reference_book(root: &Path) {
    let book_dir = root.join("refsrc").join("tiny");
    fs::create_dir_all(&book_dir).expect("mkdir refsrc/tiny");
    fs::write(book_dir.join("book.toml"), DISTILL_TINY_BOOK_TOML).expect("write book.toml");
    fs::write(book_dir.join("source.md"), DISTILL_TINY_SOURCE).expect("write source.md");
    let selection = LibrarySelection {
        data_dir: Some(root.to_path_buf()),
        library: None,
    };
    let args = DistillBuildArgs {
        paths: vec![book_dir],
        recursive: false,
        dry_run: false,
        retention_threshold: 0.10,
        no_retention_check: false,
        no_audit_write: false,
    };
    bookrack_cli::distill_cmd::run(&selection, DistillAction::Build(args))
        .await
        .expect("distill build");
    assert!(
        root.join("reference.db").is_file(),
        "distill build leaves the reference store at the data root"
    );
}

/// Two registered libraries, `alpha` with the reference book and `beta`
/// without, and a ready daemon under `alpha`.
async fn serve_alpha_and_beta() -> (Sandbox, DaemonProcess) {
    let sandbox = Sandbox::new();
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    build_tiny_reference_book(&alpha).await;
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );

    let lock_path = sandbox.tty_lock_path();
    let mut daemon_cmd = Command::from(
        bookrack_cmd!(&sandbox)
            .without_data_dir()
            .ollama_url(EmbedStub::shared().url())
            .build(),
    );
    daemon_cmd.args(["--library", "alpha", "run"]);
    let daemon = DaemonProcess::spawn(daemon_cmd)
        .expect("spawn bookrack run")
        .wait_ready(&lock_path, Duration::from_secs(20))
        .await
        .expect("bookrack run did not become ready");
    (sandbox, daemon)
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn bookrack(sandbox: &Sandbox, args: &[&str]) -> Run {
    let mut cmd = Command::from(bookrack_cmd!(sandbox).without_data_dir().build());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().await.expect("run bookrack");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The refusal for an id the library does not hold: exit `2`, the
/// three parts on stderr naming `id` and `library` in the summary and
/// `hint_names` in the hint, nothing on stdout. Under `--json` the
/// reporter writes the parts as a JSON object, so the assertions read
/// the words, not the human layout.
fn assert_not_found(run: &Run, args: &[&str], id: &str, library: &str, hint_names: &str) {
    assert_eq!(
        run.code,
        Some(2),
        "{args:?}: stdout={:?} stderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.is_empty(),
        "{args:?}: a missing item is not a body to print: {:?}",
        run.stdout
    );
    let plain = format!("in library \"{library}\"");
    let escaped = format!("in library \\\"{library}\\\"");
    assert!(
        run.stderr.contains("no item")
            && (run.stderr.contains(&plain) || run.stderr.contains(&escaped)),
        "{args:?}: the summary names the library: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains(id),
        "{args:?}: the summary names the id: {}",
        run.stderr
    );
    assert!(
        run.stderr.lines().count() >= 3 && run.stderr.contains("hint"),
        "{args:?}: the refusal carries its detail and hint: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains(hint_names),
        "{args:?}: the hint names the listing for the kind: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("internal"),
        "{args:?}: a missing item is not a fault: {}",
        run.stderr
    );
}

#[tokio::test]
async fn a_keyed_reference_id_reads_one_entry_and_a_missing_one_is_refused() {
    let (sandbox, daemon) = serve_alpha_and_beta().await;

    let json = bookrack(
        &sandbox,
        &[
            "--json",
            "--library",
            "alpha",
            "show",
            "reference:tiny/smith",
        ],
    )
    .await;
    assert_eq!(json.code, Some(0), "stderr={}", json.stderr);
    let body: Value = serde_json::from_str(&json.stdout)
        .unwrap_or_else(|e| panic!("the payload is not JSON ({e}): {}", json.stdout));
    assert_eq!(body["entry_key"], "smith", "{body}");
    assert_eq!(body["hits"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body["hits"][0]["headword"], "Smith", "{body}");
    assert_eq!(body["hits"][0]["book_slug"], "tiny", "{body}");

    let card = bookrack(
        &sandbox,
        &["--library", "alpha", "show", "reference:tiny/Smith"],
    )
    .await;
    assert_eq!(card.code, Some(0), "stderr={}", card.stderr);
    assert!(
        card.stdout.contains("headword") && card.stdout.contains("Smith"),
        "the card rows the headword:\n{}",
        card.stdout
    );
    assert!(
        card.stdout.contains("matched as") && card.stdout.contains("smith"),
        "a headword as written is matched through its latin form, and the card says so:\n{}",
        card.stdout
    );
    assert!(
        card.stdout.trim_end().ends_with("library: alpha"),
        "the card sits over the library it came from:\n{}",
        card.stdout
    );

    let nobody_args = ["--library", "alpha", "show", "reference:tiny/nobody"];
    let nobody = bookrack(&sandbox, &nobody_args).await;
    assert_not_found(
        &nobody,
        &nobody_args,
        "reference:tiny/nobody",
        "alpha",
        "distill list",
    );

    let whole = bookrack(&sandbox, &["--library", "alpha", "show", "reference:tiny"]).await;
    assert_eq!(whole.code, Some(2), "stderr={}", whole.stderr);
    assert!(
        whole.stderr.contains("cannot show a whole reference book")
            && whole.stderr.contains("reference:<slug>/<key>"),
        "{}",
        whole.stderr
    );
    assert!(whole.stdout.is_empty(), "{:?}", whole.stdout);

    let no_store_args = ["--library", "beta", "show", "reference:tiny/smith"];
    let no_store = bookrack(&sandbox, &no_store_args).await;
    assert_not_found(
        &no_store,
        &no_store_args,
        "reference:tiny/smith",
        "beta",
        "distill list",
    );

    daemon
        .stop(Duration::from_secs(5))
        .await
        .expect("daemon stops on SIGTERM");
}

#[tokio::test]
async fn a_book_or_paper_id_the_library_does_not_hold_is_refused() {
    let (sandbox, daemon) = serve_alpha_and_beta().await;

    let cases: [(&[&str], &str, &str); 3] = [
        (&["show", "book:999"], "book:999", "`bookrack list`"),
        (
            &["show", "paper:999"],
            "paper:999",
            "`bookrack papers list`",
        ),
        (
            &["papers", "show", "999"],
            "paper:999",
            "`bookrack papers list`",
        ),
    ];
    for (tail, id, hint_names) in cases {
        for json in [false, true] {
            let mut args: Vec<&str> = Vec::new();
            if json {
                args.push("--json");
            }
            args.extend_from_slice(&["--library", "alpha"]);
            args.extend_from_slice(tail);
            let run = bookrack(&sandbox, &args).await;
            assert_not_found(&run, &args, id, "alpha", hint_names);
        }
    }

    daemon
        .stop(Duration::from_secs(5))
        .await
        .expect("daemon stops on SIGTERM");
}
