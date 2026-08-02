// SPDX-License-Identifier: Apache-2.0

//! `bookrack config knobs`: the inventory of every knob this build has.
//!
//! What separates it from `config effective` is that it answers about
//! the binary, not the machine. So the tests here drive it under an
//! environment and a data root that would move the report, and assert
//! the inventory does not move — a command that merely printed the same
//! rows as the report would pass a happy-path test and fail every one
//! below.

#![cfg(unix)]

use std::process::{Output, Stdio};

use bookrack_test_support::{Sandbox, bookrack_cmd};
use eyre::Result;

/// Run `config knobs` with the given extra arguments and environment.
async fn run(sandbox: &Sandbox, args: &[&str], extra: &[(&str, &str)]) -> Result<Output> {
    let mut spawn = bookrack_cmd!(sandbox).without_data_dir();
    for (key, value) in extra {
        spawn = spawn.extra_env(key, value);
    }
    let out = tokio::process::Command::from(spawn.build())
        .args(["config", "knobs"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    Ok(out)
}

/// Parse the JSON inventory, failing with both streams when it is not
/// JSON.
fn inventory(out: &Output) -> Result<serde_json::Value> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).map_err(|e| {
        eyre::eyre!(
            "config knobs --json did not print an inventory ({e}); stdout={stdout} stderr={}",
            String::from_utf8_lossy(&out.stderr),
        )
    })
}

/// The entry for one key.
fn knob(inventory: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    inventory["knobs"]
        .as_array()?
        .iter()
        .find(|k| k["key"] == key)
        .cloned()
}

/// The inventory reports the compiled-in default even where the
/// environment sets something else.
///
/// The discriminating test of the whole command: `config effective`
/// under this environment reports 99, and an inventory that shared its
/// resolution would too. What the reader is owed here is the value the
/// knob has when nobody touches it.
#[tokio::test]
async fn the_inventory_reports_the_default_not_the_environment() -> Result<()> {
    let sandbox = Sandbox::new();
    let out = run(
        &sandbox,
        &["--json"],
        &[
            ("BOOKRACK_SEARCH_TOP_K", "99"),
            ("BOOKRACK_MCP_ADDR", "127.0.0.1:59999"),
        ],
    )
    .await?;

    assert_eq!(out.status.code(), Some(0));
    let inventory = inventory(&out)?;

    let top_k = knob(&inventory, "search.top_k").expect("no search.top_k entry");
    assert_eq!(
        top_k["default"], "5",
        "the inventory took its value from the environment: {top_k}"
    );
    assert_eq!(top_k["default_layer"], "default");

    let addr = knob(&inventory, "mcp.addr").expect("no mcp.addr entry");
    assert_eq!(addr["default"], "127.0.0.1:8765", "{addr}");

    Ok(())
}

/// A data root that will not resolve is not this command's problem: it
/// reads no root, so it answers in full and succeeds. `config
/// effective` under the same conditions exits 2 with a diagnostic, and
/// the difference is the point — one reports a machine, the other a
/// build.
#[tokio::test]
async fn a_broken_data_root_does_not_reach_the_inventory() -> Result<()> {
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
        Some(0),
        "the inventory needs no root, so a bad one cannot fail it; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let inventory = inventory(&out)?;
    let data_dir = knob(&inventory, "data_dir").expect("no data_dir entry");
    assert!(
        data_dir["default"].is_null(),
        "no layer backstops the data root, so the inventory must claim no default: {data_dir}"
    );
    assert!(
        data_dir["settable_at"]
            .as_array()
            .is_some_and(|sites| sites.iter().any(|s| s["site"] == "BOOKRACK_DATA_DIR")),
        "the inventory drops the variable that selects a root: {data_dir}"
    );

    Ok(())
}

/// Every knob names where it can be set, including the ones no variable
/// reaches. A `settable_at` that were empty would make the inventory a
/// list of names rather than an answer.
#[tokio::test]
async fn every_knob_names_at_least_one_place_it_can_be_set() -> Result<()> {
    let sandbox = Sandbox::new();
    let inventory = inventory(&run(&sandbox, &["--json"], &[]).await?)?;

    let knobs = inventory["knobs"].as_array().expect("no knobs array");
    assert!(!knobs.is_empty(), "the inventory is empty");
    for knob in knobs {
        let sites = knob["settable_at"].as_array();
        assert!(
            sites.is_some_and(|s| !s.is_empty()),
            "knob {} names nowhere it can be set: {knob}",
            knob["key"]
        );
    }

    // The file-only reranker knobs are the case a variable-keyed
    // inventory would lose: they have no variable at all, and dropping
    // them would leave `config.toml` keys undocumented by the one
    // surface that claims to list every knob.
    let ctx = knob(&inventory, "reranker.ctx").expect("no reranker.ctx entry");
    assert!(
        ctx["variable"].is_null(),
        "reranker.ctx has no variable: {ctx}"
    );
    assert!(
        ctx["settable_at"]
            .as_array()
            .is_some_and(|sites| sites.iter().any(|s| s["layer"] == "file")),
        "reranker.ctx must still name the file that sets it: {ctx}"
    );

    Ok(())
}

/// The native dependencies carry the variable that points at each one.
/// They have no priority chain, so they are not rows — but an operator
/// asking what can be configured needs them, and leaving them out sends
/// that reader to the source.
#[tokio::test]
async fn the_inventory_names_every_native_dependency_and_its_variable() -> Result<()> {
    let sandbox = Sandbox::new();
    let inventory = inventory(&run(&sandbox, &["--json"], &[]).await?)?;

    let deps = inventory["native_dependencies"]
        .as_array()
        .expect("no native_dependencies array");
    let named: Vec<&str> = deps.iter().filter_map(|d| d["name"].as_str()).collect();
    assert_eq!(named, vec!["pdfium", "llama_server", "reranker_model"]);

    for dep in deps {
        assert!(
            dep["variable"]
                .as_str()
                .is_some_and(|v| v.starts_with("BOOKRACK_")),
            "dependency {} names no variable to point it at: {dep}",
            dep["name"]
        );
    }

    Ok(())
}

/// One inventory feeding two renderers, as `config effective` does for
/// its report.
#[tokio::test]
async fn the_human_and_json_renderings_agree() -> Result<()> {
    let sandbox = Sandbox::new();
    let json = inventory(&run(&sandbox, &["--json"], &[]).await?)?;
    let addr = knob(&json, "mcp.addr").expect("no mcp.addr entry")["default"]
        .as_str()
        .expect("default is not a string")
        .to_string();

    let human = run(&sandbox, &[], &[]).await?;
    let text = String::from_utf8_lossy(&human.stdout);
    let line = text
        .lines()
        .find(|l| l.contains("mcp.addr"))
        .unwrap_or_else(|| panic!("no mcp.addr line in:\n{text}"));

    assert!(
        line.contains(&addr),
        "the human table says something the JSON does not: line={line:?} json={addr:?}"
    );

    Ok(())
}
