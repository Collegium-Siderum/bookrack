// SPDX-License-Identifier: Apache-2.0

//! `bookrack config effective`: the offline report of what the
//! configuration resolves to and where each value came from.
//!
//! The command's reason for existing is the case where nothing else
//! works — a data root that will not resolve, a daemon that will not
//! start. So the load-bearing test here is not the happy path but the
//! broken one: the table has to come out anyway, and say which part it
//! could not answer.

#![cfg(unix)]

use std::process::{Output, Stdio};

use bookrack_test_support::{Sandbox, bookrack_cmd};
use eyre::Result;

/// Run `config effective` with the given extra arguments and
/// environment, returning the raw output.
async fn run(sandbox: &Sandbox, args: &[&str], extra: &[(&str, &str)]) -> Result<Output> {
    let mut spawn = bookrack_cmd!(sandbox).without_data_dir();
    for (key, value) in extra {
        spawn = spawn.extra_env(key, value);
    }
    let out = tokio::process::Command::from(spawn.build())
        .args(["config", "effective"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    Ok(out)
}

/// Parse the JSON report, failing with both streams when it is not
/// JSON — the whole claim under test is that a report comes out.
fn report(out: &Output) -> Result<serde_json::Value> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).map_err(|e| {
        eyre::eyre!(
            "config effective --json did not print a report ({e}); stdout={stdout} stderr={}",
            String::from_utf8_lossy(&out.stderr),
        )
    })
}

/// The rows of a parsed report.
fn rows(report: &serde_json::Value) -> Vec<serde_json::Value> {
    report["rows"].as_array().cloned().unwrap_or_default()
}

/// The row for one key.
fn row(report: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    rows(report).into_iter().find(|r| r["key"] == key)
}

/// The `.env` reach section of a parsed report.
fn foreign(report: &serde_json::Value) -> Vec<serde_json::Value> {
    report["dotenv_foreign"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The status the report gives one variable of that section.
fn foreign_status(report: &serde_json::Value, key: &str) -> Option<String> {
    foreign(report)
        .into_iter()
        .find(|e| e["key"] == key)
        .and_then(|e| e["status"].as_str().map(str::to_string))
}

/// `.env` writes the real process environment, so it reaches variables
/// no knob row can account for. The report names them: without this
/// section a proxy or a `HOME` the file installed is invisible on every
/// configuration surface the program has.
///
/// Both halves are asserted, because they are different facts about the
/// file: a line that reached the environment, and a line the
/// environment got to first and that was therefore discarded.
#[tokio::test]
async fn the_report_names_the_process_variables_dotenv_set_outside_the_prefix() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("main");
    std::fs::write(
        sandbox.cwd().join(".env"),
        format!(
            "BOOKRACK_DATA_DIR={}\nEXAMPLE_FOREIGN_KNOB=from-the-file\n\
             EXAMPLE_ECLIPSED_KNOB=from-the-file\n",
            root.display()
        ),
    )?;

    let out = run(
        &sandbox,
        &["--json"],
        &[("EXAMPLE_ECLIPSED_KNOB", "from-the-environment")],
    )
    .await?;
    let report = report(&out)?;

    assert_eq!(
        foreign_status(&report, "EXAMPLE_FOREIGN_KNOB").as_deref(),
        Some("set"),
        "a variable the file installed in the process is unreported: {report}"
    );
    assert_eq!(
        foreign_status(&report, "EXAMPLE_ECLIPSED_KNOB").as_deref(),
        Some("eclipsed"),
        "a line the environment outranked is unreported: {report}"
    );
    assert!(
        !foreign(&report)
            .iter()
            .any(|e| e["key"] == "BOOKRACK_DATA_DIR"),
        "a knob with a row of its own must not be repeated as a foreign \
         variable: {report}"
    );

    let human = run(
        &sandbox,
        &[],
        &[("EXAMPLE_ECLIPSED_KNOB", "from-the-environment")],
    )
    .await?;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("EXAMPLE_FOREIGN_KNOB"),
        "the human report omits what the JSON one carries:\n{text}"
    );

    Ok(())
}

/// A data root that does not exist takes the table down with it only if
/// the table was built the wrong way round. Every knob resolved before
/// or without a root still has an answer, and the ones that needed the
/// root say so instead of vanishing.
#[tokio::test]
async fn the_table_survives_a_data_root_that_does_not_exist() -> Result<()> {
    let sandbox = Sandbox::new();
    let missing = sandbox.path().join("nope");
    let out = run(
        &sandbox,
        &["--json"],
        &[("BOOKRACK_DATA_DIR", &missing.display().to_string())],
    )
    .await?;

    assert_eq!(
        out.status.code(),
        Some(2),
        "a root the operator named wrongly is a user error; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report = report(&out)?;
    assert!(
        !report["problem"].is_null(),
        "a failed resolution must be stated, not left to be inferred: {report}"
    );

    for key in [
        "no_dotenv",
        "mcp.addr",
        "log.directive",
        "runtime_dir",
        "confirm.timeout_secs",
    ] {
        let row = row(&report, key)
            .unwrap_or_else(|| panic!("row {key} is missing from a report with a bad root"));
        assert!(
            !row["value"].is_null(),
            "row {key} never needed a data root, so it must still have a value: {row}"
        );
    }

    let library_rows: Vec<_> = rows(&report)
        .into_iter()
        .filter(|r| r["reach"] == "library")
        .collect();
    assert!(
        !library_rows.is_empty(),
        "the library-scoped rows vanished rather than reporting what they could not read"
    );

    Ok(())
}

/// Two libraries do not bleed into each other: the row reports the
/// value of the root actually selected, and names which root that was.
#[tokio::test]
async fn a_library_scoped_row_names_the_root_it_was_read_from() -> Result<()> {
    let sandbox = Sandbox::new();
    let a = sandbox.data_root("a");
    let b = sandbox.data_root("b");
    std::fs::write(a.join("config.toml"), "[search]\ntop_k = 5\n")?;
    std::fs::write(b.join("config.toml"), "[search]\ntop_k = 7\n")?;
    sandbox.write_registry_entries(None, &[("a", &a), ("b", &b)]);

    let out = run(&sandbox, &["--json", "--library", "b"], &[]).await?;
    let report = report(&out)?;
    let top_k = row(&report, "search.top_k").expect("no search.top_k row");

    assert_eq!(
        top_k["value"], "7",
        "the row took its value from the wrong library: {top_k}"
    );
    assert_eq!(top_k["reach"], "library");
    assert!(
        top_k["scope_instance"]
            .as_str()
            .is_some_and(|s| s.contains(&b.display().to_string())),
        "the row does not name the root it was read from: {top_k}"
    );

    Ok(())
}

/// One resolution feeding two renderers. A second code path computing
/// the same answer is exactly the failure this command exists to make
/// visible, so it must not be committed inside the command itself.
#[tokio::test]
async fn the_human_and_json_renderings_agree_on_the_winner() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("main");
    std::fs::write(root.join("config.toml"), "[search]\ntop_k = 11\n")?;
    let env = [("BOOKRACK_DATA_DIR", root.display().to_string())];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let json = report(&run(&sandbox, &["--json"], &env).await?)?;
    let json_value = row(&json, "search.top_k").expect("no search.top_k row")["value"]
        .as_str()
        .expect("value is not a string")
        .to_string();

    let human = run(&sandbox, &[], &env).await?;
    let text = String::from_utf8_lossy(&human.stdout);
    let line = text
        .lines()
        .find(|l| l.contains("search.top_k"))
        .unwrap_or_else(|| panic!("no search.top_k line in:\n{text}"));

    assert!(
        line.contains(&json_value),
        "the human table says something the JSON does not: line={line:?} json={json_value:?}"
    );

    Ok(())
}

/// `--quiet` suppresses the report without suppressing the verdict: a
/// script that only reads the exit code still learns the root is bad.
#[tokio::test]
async fn quiet_prints_nothing_and_still_signals() -> Result<()> {
    let sandbox = Sandbox::new();
    let missing = sandbox.path().join("nope");
    let out = run(
        &sandbox,
        &["--quiet"],
        &[("BOOKRACK_DATA_DIR", &missing.display().to_string())],
    )
    .await?;

    assert_eq!(out.status.code(), Some(2));
    assert!(
        out.stdout.is_empty(),
        "--quiet printed {} bytes of stdout",
        out.stdout.len()
    );

    Ok(())
}
