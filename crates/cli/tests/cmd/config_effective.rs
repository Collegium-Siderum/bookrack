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
    let mut spawn = bookrack_cmd!(sandbox);
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

/// `.env` reaches variables no knob row can account for. The report
/// names them: without this section a proxy the file installed is
/// invisible on every configuration surface the program has.
///
/// Both halves are asserted, because they are different facts about the
/// file: a line that reached the environment, and a line the
/// environment got to first and that was therefore discarded. Both
/// names are admitted ones, so what is under test is the reporting and
/// not the admission rule.
#[tokio::test]
async fn the_report_names_the_process_variables_dotenv_set_outside_the_prefix() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("main");
    std::fs::write(
        sandbox.cwd().join(".env"),
        format!(
            "BOOKRACK_DATA_DIR={}\nHTTP_PROXY=http://127.0.0.1:9\nNO_COLOR=from-the-file\n",
            root.display()
        ),
    )?;
    let env = [("NO_COLOR", "from-the-environment")];

    let out = run(&sandbox, &["--json"], &env).await?;
    let report = report(&out)?;

    assert_eq!(
        foreign_status(&report, "HTTP_PROXY").as_deref(),
        Some("set"),
        "a variable the file installed in the process is unreported: {report}"
    );
    assert_eq!(
        foreign_status(&report, "NO_COLOR").as_deref(),
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

    let human = run(&sandbox, &[], &env).await?;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("HTTP_PROXY"),
        "the human report omits what the JSON one carries:\n{text}"
    );

    Ok(())
}

/// A `BOOKRACK_` variable no knob of this build reads is listed with
/// where the process got it, rather than accepted in silence: a
/// misspelt name and a knob only another build has both look like
/// this. The list is compared whole, so every variable a knob does
/// read — the sandbox's own included — is shown absent from it, and
/// the report still exits 0: the variable is reported, not refused.
/// A name the file also writes but the environment already carried is
/// credited to the environment, which is where the value came from.
#[tokio::test]
async fn a_prefixed_variable_no_knob_reads_is_listed_with_its_source() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("main");
    std::fs::write(
        sandbox.cwd().join(".env"),
        format!(
            "BOOKRACK_DATA_DIR={}\nBOOKRACK_FROM_THE_FILE=1\nBOOKRACK_SERACH_TOP_K=9\n",
            root.display()
        ),
    )?;
    let env = [("BOOKRACK_SERACH_TOP_K", "7")];

    let out = run(&sandbox, &["--json"], &env).await?;
    let report = report(&out)?;
    assert_eq!(
        out.status.code(),
        Some(0),
        "an unclaimed variable is reported, not refused: {report}"
    );
    assert_eq!(
        report["unclaimed_variables"],
        serde_json::json!([
            { "key": "BOOKRACK_FROM_THE_FILE", "source": "dotenv" },
            { "key": "BOOKRACK_SERACH_TOP_K", "source": "environment" },
        ]),
        "{report}"
    );

    let human = run(&sandbox, &[], &env).await?;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("BOOKRACK_SERACH_TOP_K") && text.contains("BOOKRACK_FROM_THE_FILE"),
        "the human report omits what the JSON one carries:\n{text}"
    );

    Ok(())
}

/// A data root that `--library` outranked is reported as a losing layer
/// holding what it named, credited to whichever of the environment and
/// `.env` set it. Run through the binary because the `.env` half rests
/// on the loader having written the file's value into the process
/// environment, which only a real load shows.
#[tokio::test]
async fn a_data_root_a_flag_outranked_is_reported_where_it_was_set() -> Result<()> {
    for from_file in [false, true] {
        let sandbox = Sandbox::new();
        let a = sandbox.data_root("a");
        let b = sandbox.data_root("b");
        sandbox.write_registry_entries(None, &[("a", &a), ("b", &b)]);
        let outranked = a.display().to_string();
        let env: Vec<(&str, &str)> = if from_file {
            std::fs::write(
                sandbox.cwd().join(".env"),
                format!("BOOKRACK_DATA_DIR={outranked}\n"),
            )?;
            Vec::new()
        } else {
            vec![("BOOKRACK_DATA_DIR", outranked.as_str())]
        };
        let (layer, site_suffix) = if from_file {
            ("dotenv", "/.env")
        } else {
            ("environment", "BOOKRACK_DATA_DIR")
        };

        let report = report(&run(&sandbox, &["--json", "--library", "b"], &env).await?)?;
        let data_dir = row(&report, "data_dir").expect("no data_dir row");
        assert_eq!(data_dir["layer"], "flag", "{data_dir}");
        assert!(
            data_dir["value"]
                .as_str()
                .is_some_and(|v| v == b.display().to_string()),
            "the flag's root did not win: {data_dir}"
        );
        let shadowed: Vec<(String, bool, String)> = data_dir["shadowed"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|s| {
                (
                    s["layer"].as_str().unwrap_or_default().to_string(),
                    s["site"].as_str().is_some_and(|x| x.ends_with(site_suffix)),
                    s["value"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        assert_eq!(
            shadowed,
            vec![(layer.to_string(), true, outranked.clone())],
            "set in the {layer} layer, the outranked root is missing or misplaced: {data_dir}"
        );

        let human = run(&sandbox, &["--library", "b"], &env).await?;
        let text = String::from_utf8_lossy(&human.stdout);
        assert!(
            text.lines().any(|l| l.contains("data_dir")
                && l.contains("<-")
                && l.contains(layer)
                && l.contains(&outranked)),
            "the human report omits what the JSON one carries:\n{text}"
        );
    }

    Ok(())
}

/// A variable outside the admitted set is read out of the file and
/// then dropped, rather than installed in the process.
///
/// The home directory is the case with a consequence this report can
/// show: the daemon state directory derives from it, so a `HOME` or an
/// `XDG_DATA_HOME` that reached the environment turns up as the prefix
/// of the `daemon_state_dir` row. The row is the independent witness
/// here — the reach section alone would only be the loader repeating
/// its own claim.
#[tokio::test]
async fn a_variable_outside_the_admitted_set_never_reaches_the_process() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("main");
    let evil_home = sandbox.path().join("evil-home");
    let evil_xdg = sandbox.path().join("evil-xdg");
    std::fs::write(
        sandbox.cwd().join(".env"),
        format!(
            "BOOKRACK_DATA_DIR={}
HOME={}
XDG_DATA_HOME={}
CI=1
             EXAMPLE_FOREIGN_KNOB=from-the-file
",
            root.display(),
            evil_home.display(),
            evil_xdg.display(),
        ),
    )?;

    // Removed rather than left alone: a variable the child already
    // carries outranks the file's line, and this would pass without the
    // admission rule doing anything. The state directory's own variable
    // goes, so the row falls to the platform directory; then the one
    // variable that directory derives from on this platform goes too,
    // so the file's line is the only candidate left for it.
    let platform_home = if cfg!(target_os = "macos") {
        "HOME"
    } else {
        "XDG_DATA_HOME"
    };
    let spawn = bookrack_cmd!(&sandbox)
        .without_env("CI")
        .without_env("EXAMPLE_FOREIGN_KNOB")
        .without_env(bookrack_config::DAEMON_STATE_DIR_ENV)
        .without_env(platform_home);
    let out = tokio::process::Command::from(spawn.build())
        .args(["config", "effective", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    let report = report(&out)?;

    let state_dir = row(&report, "daemon_state_dir").expect("no daemon_state_dir row");
    let value = state_dir["value"].as_str().unwrap_or_default();
    assert!(
        !value.is_empty(),
        "premise: the state directory resolves: {state_dir}"
    );
    for evil in [&evil_home, &evil_xdg] {
        assert!(
            !std::path::Path::new(value).starts_with(evil),
            "the file's home directory reached the process: {state_dir}"
        );
    }

    for key in ["HOME", "XDG_DATA_HOME", "CI", "EXAMPLE_FOREIGN_KNOB"] {
        assert_eq!(
            foreign_status(&report, key).as_deref(),
            Some("rejected"),
            "{key} must be reported as read and dropped: {report}"
        );
    }

    Ok(())
}

/// No variable redirects the search for a native component. With a
/// stand-in in each managed directory and every retired override
/// pointing somewhere else, the report finds the managed copies, names
/// no override, and lists the variables as retired.
#[tokio::test]
async fn the_native_search_ignores_the_retired_override_variables() -> Result<()> {
    let sandbox = Sandbox::new();
    let library = super::setup_cmd::seed_pdfium(&sandbox);
    let llama = bookrack_config::llama_server_pin::pinned_llama_server().map(|pin| {
        let path = sandbox
            .managed_data_dir()
            .join("bookrack")
            .join("llama-server")
            .join(bookrack_config::llama_server_pin::LLAMA_SERVER_RELEASE_TAG)
            .join(pin.exe_in_archive);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("managed directory");
        std::fs::write(&path, b"stand-in").expect("stand-in executable");
        path
    });
    let elsewhere = sandbox.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere)?;
    let elsewhere = elsewhere.display().to_string();
    let env = [
        ("BOOKRACK_PDFIUM_LIB", elsewhere.as_str()),
        ("BOOKRACK_LLAMA_SERVER_BIN", elsewhere.as_str()),
        ("BOOKRACK_RERANKER_MODEL", elsewhere.as_str()),
        ("BOOKRACK_REQUIRE_PDFIUM", "1"),
    ];

    let out = run(&sandbox, &["--json"], &env).await?;
    let report = report(&out)?;
    let deps = report["native_dependencies"]
        .as_array()
        .expect("native_dependencies is an array");
    let dep = |name: &str| {
        deps.iter()
            .find(|d| d["name"] == name)
            .unwrap_or_else(|| panic!("no {name} entry: {report}"))
    };
    for name in ["pdfium", "llama_server", "reranker_model"] {
        assert!(
            dep(name)["override_site"].is_null(),
            "{name} still names an override: {}",
            dep(name)
        );
    }

    let pdfium = dep("pdfium");
    assert_eq!(
        pdfium["path"],
        library.parent().expect("parent").display().to_string(),
        "the variable moved the PDFium search: {pdfium}"
    );
    assert_eq!(pdfium["layer"], "platform", "{pdfium}");
    assert_eq!(pdfium["site"], "managed directory", "{pdfium}");
    if let Some(llama) = llama {
        let found = dep("llama_server");
        assert_eq!(
            found["path"],
            llama.display().to_string(),
            "the variable moved the llama-server search: {found}"
        );
        assert_eq!(found["layer"], "platform", "{found}");
    }

    assert!(
        row(&report, "pdfium.required").is_none(),
        "the test-harness row is still reported: {report}"
    );
    let retired: Vec<&str> = report["retired_variables"]
        .as_array()
        .expect("retired_variables is an array")
        .iter()
        .filter_map(|var| var["key"].as_str())
        .collect();
    for (name, _) in env {
        assert!(
            retired.contains(&name),
            "{name} is set and not reported as retired: {report}"
        );
    }
    assert_eq!(
        report["unclaimed_variables"],
        serde_json::json!([]),
        "{report}"
    );

    let human = run(&sandbox, &[], &env).await?;
    let text = String::from_utf8_lossy(&human.stdout);
    let cells = |line: &str| -> Vec<String> {
        line.trim_matches(|c: char| !c.is_ascii_graphic())
            .split_whitespace()
            .map(str::to_string)
            .collect()
    };
    let line = text
        .lines()
        .map(cells)
        .find(|cells| cells.first().is_some_and(|c| c == "pdfium"))
        .unwrap_or_else(|| panic!("no pdfium line:\n{text}"));
    assert_eq!(
        line.last().map(String::as_str),
        Some("-"),
        "the override column still names something: {line:?}"
    );
    Ok(())
}

/// The admitted names are admitted: a proxy is the reason `.env` is
/// the only configuration surface a desktop launch has, and it still
/// reaches the process.
#[tokio::test]
async fn an_admitted_variable_outside_the_prefix_still_reaches_the_process() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("main");
    std::fs::write(
        sandbox.cwd().join(".env"),
        format!(
            "BOOKRACK_DATA_DIR={}\nHTTP_PROXY=http://127.0.0.1:9\n",
            root.display()
        ),
    )?;

    let out = run(&sandbox, &["--json"], &[]).await?;
    let report = report(&out)?;

    assert_eq!(
        foreign_status(&report, "HTTP_PROXY").as_deref(),
        Some("set"),
        "a proxy written into .env must still take effect: {report}"
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

/// A variable an earlier release read is neither a knob nor an unknown
/// name: it is listed as retired, with what to do about it and where
/// the process got it, and it resolves nothing.
#[tokio::test]
async fn a_retired_variable_is_reported_as_retired_and_not_as_unclaimed() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("main");
    std::fs::write(
        sandbox.cwd().join(".env"),
        format!(
            "BOOKRACK_DATA_DIR={}\nBOOKRACK_VECTORS_REFINE_FACTOR=3\n",
            root.display()
        ),
    )?;
    let env = [("BOOKRACK_VECTORS_NPROBES", "8")];

    let out = run(&sandbox, &["--json"], &env).await?;
    let report = report(&out)?;
    assert_eq!(out.status.code(), Some(0), "{report}");
    assert!(
        row(&report, "vectors.nprobes").is_none(),
        "a retired variable still resolves a row: {report}"
    );
    let retired = report["retired_variables"]
        .as_array()
        .expect("retired_variables is an array");
    let seen: Vec<(&str, &str)> = retired
        .iter()
        .map(|var| {
            (
                var["key"].as_str().expect("key"),
                var["source"].as_str().expect("source"),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("BOOKRACK_VECTORS_NPROBES", "environment"),
            ("BOOKRACK_VECTORS_REFINE_FACTOR", "dotenv"),
        ],
        "{report}"
    );
    for var in retired {
        assert!(
            !var["help"].as_str().expect("help").is_empty(),
            "a retired variable says what to do: {var}"
        );
    }
    assert_eq!(
        report["unclaimed_variables"],
        serde_json::json!([]),
        "a retired variable is not also unclaimed: {report}"
    );

    let human = run(&sandbox, &[], &env).await?;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("BOOKRACK_VECTORS_NPROBES") && text.contains("retired"),
        "the human report omits the retired variable:\n{text}"
    );
    Ok(())
}

/// A clean environment still carries the section, empty.
#[tokio::test]
async fn the_retired_section_is_present_when_empty() -> Result<()> {
    let sandbox = Sandbox::new();
    let out = run(&sandbox, &["--json"], &[]).await?;
    let report = report(&out)?;
    assert_eq!(
        report["retired_variables"],
        serde_json::json!([]),
        "{report}"
    );
    Ok(())
}

/// A library's search preferences are its own: under one environment
/// two libraries report their own values, and a variable an earlier
/// release read for them moves neither.
#[tokio::test]
async fn two_libraries_keep_their_own_top_k_under_one_environment() -> Result<()> {
    let sandbox = Sandbox::new();
    let a = sandbox.data_root("a");
    let b = sandbox.data_root("b");
    sandbox.write_registry_entries(None, &[("a", &a), ("b", &b)]);
    std::fs::write(a.join("config.toml"), "[search]\ntop_k = 2\n")?;
    let env = [("BOOKRACK_SEARCH_TOP_K", "9")];

    for (library, value, layer) in [("a", "2", "file"), ("b", "5", "default")] {
        let out = run(&sandbox, &["--library", library, "--json"], &env).await?;
        let report = report(&out)?;
        let top_k = row(&report, "search.top_k").expect("search.top_k row");
        assert_eq!(top_k["value"], value, "library {library}: {top_k}");
        assert_eq!(top_k["layer"], layer, "library {library}: {top_k}");
        let sites: Vec<&str> = top_k["chain"]
            .as_array()
            .expect("chain")
            .iter()
            .filter_map(|site| site["layer"].as_str())
            .collect();
        assert_eq!(sites, ["file", "default"], "library {library}: {top_k}");
    }
    Ok(())
}

/// The Ollama endpoint is the machine's. A value a library's
/// `config.toml` still carries is reported as a retired key and
/// resolves nothing.
#[tokio::test]
async fn a_retired_ollama_url_in_config_toml_is_not_used() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("main");
    std::fs::write(
        root.join("config.toml"),
        "ollama_url = \"http://library-host:1\"\n",
    )?;
    let data_dir = root.display().to_string();

    let out = run(&sandbox, &["--data-dir", &data_dir, "--json"], &[]).await?;
    let report = report(&out)?;
    let url = row(&report, "ollama_url").expect("ollama_url row");
    assert_eq!(
        url["value"], "http://localhost:11434",
        "the library's file moved the machine's endpoint: {url}"
    );
    assert_eq!(url["layer"], "default", "{url}");
    assert_eq!(url["reach"], "machine", "{url}");
    assert_eq!(url["read_at"], "before_resolution", "{url}");
    assert!(url.get("scope_instance").is_none(), "{url}");
    assert!(
        !url.to_string().contains("library-host"),
        "the file's value is reported nowhere on the row: {url}"
    );
    let notices: Vec<&str> = report["config_notices"]
        .as_array()
        .expect("config_notices")
        .iter()
        .filter_map(|notice| notice["key"].as_str())
        .collect();
    assert_eq!(notices, ["ollama_url"], "{report}");
    Ok(())
}

/// A user index profile declaring `model`, valid on structure alone.
fn write_profile(sandbox: &Sandbox, name: &str, model: &str) {
    let dir = sandbox.path().join("index-profiles");
    std::fs::create_dir_all(&dir).expect("profile directory");
    std::fs::write(
        dir.join(format!("{name}.toml")),
        format!(
            "schema_version = 1\nname = \"{name}\"\ndescription = \"Test profile.\"\n\n\
             [embed]\nbackend = \"ollama\"\nmodel = \"{model}\"\ndim = 1024\n\n\
             [ann]\nkind = \"ivf-pq\"\nnum_partitions = 256\nnum_sub_vectors = 128\n\
             num_bits = 8\nnprobes = 16\nrefine_factor = 10\n\n\
             [reranker]\nkind = \"none\"\n"
        ),
    )
    .expect("write profile");
}

/// A registry whose one library, `lib`, references `profile`.
fn library_referencing(sandbox: &Sandbox, profile: &str) {
    let root = sandbox.data_root("lib");
    sandbox.write_registry(&format!(
        "[libraries.lib]\ndata_dir = {:?}\nindex_profile = \"{profile}\"\n",
        root.display().to_string()
    ));
}

/// The embed model is the index profile's fact, and the row says so:
/// the profile's model at the manifest layer, the compiled-in default
/// beneath it as the value it eclipsed.
#[tokio::test]
async fn embed_model_reports_the_profile_that_declares_it() -> Result<()> {
    let sandbox = Sandbox::new();
    write_profile(&sandbox, "probe", "probe-embed-model");
    library_referencing(&sandbox, "probe");

    let out = run(&sandbox, &["--library", "lib", "--json"], &[]).await?;
    let report = report(&out)?;
    assert_eq!(out.status.code(), Some(0), "{report}");
    let model = row(&report, "embed.model").expect("embed.model row");
    assert_eq!(
        model["value"], "probe-embed-model",
        "the row ignores the profile the library declares: {model}"
    );
    assert_eq!(model["layer"], "manifest", "{model}");
    assert!(
        model["shadowed"]
            .as_array()
            .is_some_and(|lost| lost.iter().any(|s| s["layer"] == "default")),
        "the default the profile eclipsed is not shown: {model}"
    );
    assert!(model.get("note").is_none(), "{model}");
    Ok(())
}

/// A profile reference that does not resolve leaves the row at the
/// default and says why; the report is still a report.
#[tokio::test]
async fn an_unresolvable_profile_does_not_fail_the_report() -> Result<()> {
    let sandbox = Sandbox::new();
    library_referencing(&sandbox, "no-such-profile");

    let out = run(&sandbox, &["--library", "lib", "--json"], &[]).await?;
    let report = report(&out)?;
    assert_eq!(out.status.code(), Some(0), "{report}");
    let model = row(&report, "embed.model").expect("embed.model row");
    assert_eq!(model["layer"], "default", "{model}");
    let note = model["note"]
        .as_str()
        .unwrap_or_else(|| panic!("the row does not say why the profile is missing: {model}"));
    assert!(note.contains("no-such-profile"), "{note}");

    let human = run(&sandbox, &["--library", "lib"], &[]).await?;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("no-such-profile"),
        "the human report drops the reason:\n{text}"
    );
    Ok(())
}
