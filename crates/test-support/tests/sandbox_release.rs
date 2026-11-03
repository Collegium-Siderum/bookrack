// SPDX-License-Identifier: Apache-2.0

//! The variable-claim gate and the frozen state, against stand-ins.
//!
//! A stand-in answers `config knobs --json` with an inventory the test
//! wrote, so the gate's judgement can be driven to each of its exits;
//! and a bare sandbox's marker can be written frozen by hand, so every
//! command's refusal of a frozen sandbox is observed without a release
//! golden to freeze.

#![cfg(unix)]

mod common;

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use bookrack_test_support::tool::build::BuildHooks;
use bookrack_test_support::tool::{
    CreateOpts, Preset, STAGE_BINARY, STAGE_PATHS, STAGE_PRESET, Session, SessionOpts, Source,
};
use common::sandbox::{PRELUDE, TOOL, code, read_json, stderr, write_script};
use serde_json::{Value, json};

/// The `BOOKRACK_*` variables the isolation plan sets on a child.
const PLANNED: [&str; 6] = [
    "BOOKRACK_DAEMON_STATE_DIR",
    "BOOKRACK_MCP_ADDR",
    "BOOKRACK_NO_DOTENV",
    "BOOKRACK_OLLAMA_URL",
    "BOOKRACK_REGISTRY",
    "BOOKRACK_RUNTIME_DIR",
];

/// A stand-in whose `config knobs --json` claims `variables`, and whose
/// `--version` answers.
fn knobs_script(variables: &[&str]) -> String {
    let rows: Vec<String> = variables
        .iter()
        .map(|v| format!("{{\"key\":\"k\",\"variable\":\"{v}\"}}"))
        .collect();
    let inventory = format!(
        "{{\"knobs\":[{}],\"native_dependencies\":[]}}",
        rows.join(",")
    );
    knobs_script_with(&format!("printf '%s\\n' '{inventory}'; exit 0"))
}

/// A stand-in whose `config knobs` arm runs `knobs_arm`.
fn knobs_script_with(knobs_arm: &str) -> String {
    format!(
        "{}case \" $* \" in\n  *\" config knobs \"*) {knobs_arm} ;;\n  *\" --version \"*) echo \
         'stand-in 1'; exit 0 ;;\nesac\nexit 0\n",
        PRELUDE.replace("__HOOK__", "")
    )
}

struct Place {
    root: tempfile::TempDir,
}

impl Place {
    fn new() -> Place {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(root.path().join("ws")).expect("ws");
        Place { root }
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn ws(&self) -> String {
        self.root.path().join("ws").display().to_string()
    }

    fn script(&self, name: &str, text: &str) -> PathBuf {
        let path = self.root.path().join(name);
        write_script(&path, text);
        path
    }

    fn tool(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(TOOL);
        cmd.args(args).stdin(Stdio::null());
        cmd.output().expect("run bookrack-sandbox")
    }

    /// `new <dir> --preset bare --binary <script>`.
    fn new_with_binary(&self, dir: &Path, script: &Path, extra: &[&str]) -> Output {
        let dir = dir.display().to_string();
        let script = script.display().to_string();
        let ws = self.ws();
        let mut args = vec![
            "new",
            dir.as_str(),
            "--preset",
            "bare",
            "--binary",
            script.as_str(),
            "--workspace",
            ws.as_str(),
        ];
        args.extend_from_slice(extra);
        self.tool(&args)
    }
}

fn records(dir: &Path) -> Vec<Value> {
    let calls = dir.join(".calls");
    let mut ids: Vec<String> = std::fs::read_dir(&calls)
        .map(|d| {
            d.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .filter(|n| n != "last")
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids.iter()
        .map(|id| read_json(&calls.join(id).join("result.json")))
        .collect()
}

fn last_record(dir: &Path) -> Value {
    records(dir).pop().expect("a record")
}

fn signature(path: &Path) -> Vec<String> {
    let out = Command::new(TOOL)
        .args(["sig", &path.display().to_string()])
        .output()
        .expect("sig");
    assert!(out.status.success(), "sig: {}", stderr(&out));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn no_incoming(dir: &Path) {
    assert!(
        !dir.join("run/bin/bookrack.incoming").exists(),
        "the incoming copy was left behind"
    );
}

/// Processes whose command line names `script`.
fn processes_running(script: &Path) -> Vec<String> {
    let out = Command::new("pgrep")
        .args(["-fl", &script.display().to_string()])
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_binary_that_does_not_know_a_planned_variable_is_refused() {
    let place = Place::new();
    let full = place.script("full.sh", &knobs_script(&PLANNED));
    let missing: Vec<&str> = PLANNED
        .iter()
        .copied()
        .filter(|v| *v != "BOOKRACK_MCP_ADDR")
        .collect();
    let partial = place.script("partial.sh", &knobs_script(&missing));

    let ok = place.new_with_binary(&place.dir("full"), &full, &[]);
    assert_eq!(
        code(&ok),
        0,
        "a binary that claims every variable: {}",
        stderr(&ok)
    );

    let refused = place.new_with_binary(&place.dir("partial"), &partial, &[]);
    assert_eq!(code(&refused), 125, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("BOOKRACK_MCP_ADDR"),
        "the refusal names the variable: {}",
        stderr(&refused)
    );
    let record = last_record(&place.dir("partial"));
    assert_eq!(record["class"], "refused", "{record}");
    assert_eq!(record["stage"], STAGE_BINARY, "{record}");
    no_incoming(&place.dir("partial"));
}

#[test]
fn build_reruns_the_variable_gate() {
    let place = Place::new();
    let script = place.script("binary.sh", &knobs_script(&PLANNED));
    let dir = place.dir("sbx");
    let ok = place.new_with_binary(&dir, &script, &[]);
    assert_eq!(code(&ok), 0, "{}", stderr(&ok));
    let dir = dir.canonicalize().unwrap();
    let before: Value = read_json(&dir.join("run/binary.json"));

    let missing: Vec<&str> = PLANNED.iter().copied().skip(1).collect();
    write_script(&script, &knobs_script(&missing));
    let refused = place.tool(&["build", &dir.display().to_string()]);
    assert_eq!(code(&refused), 125, "{}", stderr(&refused));
    assert_eq!(last_record(&dir)["stage"], STAGE_BINARY);
    no_incoming(&dir);

    let after: Value = read_json(&dir.join("run/binary.json"));
    assert_eq!(
        after["sha256"], before["sha256"],
        "the record still names the old binary"
    );
    let out = place.tool(&["exec", &dir.display().to_string(), "--", "--version"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("stand-in 1"),
        "the old binary still runs"
    );
}

#[test]
fn a_probe_that_fails_does_not_pass_the_gate() {
    let place = Place::new();
    let cases = [
        ("exits-3", "exit 3", "exited"),
        ("not-json", "echo not json; exit 0", "not JSON"),
        ("hangs", "sleep 60; exit 0", "did not complete"),
        ("holds-stdout", "sleep 60 & exit 0", "did not complete"),
    ];
    for (name, arm, phrase) in cases {
        let script = place.script(&format!("{name}.sh"), &knobs_script_with(arm));
        let dir = place.dir(name);
        let refused = place.new_with_binary(&dir, &script, &[]);
        assert_eq!(code(&refused), 125, "{name}: {}", stderr(&refused));
        assert_eq!(last_record(&dir)["stage"], STAGE_BINARY, "{name}");
        assert!(
            stderr(&refused).contains(phrase),
            "{name}: the detail names how the probe failed: {}",
            stderr(&refused)
        );
        no_incoming(&dir);
        assert_eq!(
            processes_running(&script),
            Vec::<String>::new(),
            "{name}: the probe's processes are gone"
        );
    }
}

#[tokio::test]
async fn a_record_left_behind_the_binary_is_caught() {
    let place = Place::new();
    let script = place.script("binary.sh", &knobs_script(&PLANNED));
    let mut session = Session::create(
        &place.dir("sbx"),
        Source::File(script),
        Preset::Bare,
        CreateOpts {
            replace: false,
            workspace: Some(place.root.path().join("ws")),
        },
        SessionOpts::default(),
    )
    .await
    .unwrap_or_else(|o| panic!("create: {o:?}"));

    let failed = session
        .build_with(BuildHooks {
            after_rename: Some(Box::new(|| {
                Err(std::io::Error::other("interrupted after the rename"))
            })),
        })
        .await
        .expect_err("the injected failure comes back");
    assert_eq!(failed.stage, STAGE_BINARY, "{failed:?}");

    let refused = session
        .exec(&["--version".into()], Default::default())
        .await
        .expect_err("a binary without a matching record is refused");
    assert_eq!(refused.stage, STAGE_BINARY, "{refused:?}");
    let hint = refused
        .problem
        .as_ref()
        .and_then(|p| p.data.hint.clone())
        .unwrap_or_default();
    assert!(hint.contains("build"), "{refused:?}");

    session
        .build()
        .await
        .unwrap_or_else(|o| panic!("build: {o:?}"));
    let out = session
        .exec(&["--version".into()], Default::default())
        .await
        .unwrap_or_else(|o| panic!("exec: {o:?}"));
    assert_eq!(out.outcome.code, Some(0), "{:?}", out.outcome);
}

/// Mark the sandbox at `dir` frozen by hand.
fn forge_frozen(dir: &Path) {
    let marker_path = dir.join(".sandbox.json");
    let mut marker: Value = read_json(&marker_path);
    marker["frozen"] = json!(true);
    std::fs::write(&marker_path, serde_json::to_vec_pretty(&marker).unwrap()).expect("marker");
}

/// Take every write bit off `root`, directories after their contents.
fn chmod_a_minus_w(root: &Path) {
    let meta = std::fs::symlink_metadata(root).expect("metadata");
    if meta.is_dir() {
        for entry in std::fs::read_dir(root).expect("read_dir") {
            chmod_a_minus_w(&entry.expect("entry").path());
        }
    }
    let mode = meta.permissions().mode() & !0o222;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn writable(path: &Path) -> bool {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o200 != 0
}

#[test]
fn a_frozen_sandbox_refuses_every_write() {
    let place = Place::new();
    let script = place.script("binary.sh", &knobs_script(&PLANNED));

    // A bare sandbox is not a release golden.
    let bare = place.dir("bare");
    assert_eq!(code(&place.new_with_binary(&bare, &script, &[])), 0);
    let bare = bare.canonicalize().unwrap();
    let refused = place.tool(&["freeze", &bare.display().to_string()]);
    assert_eq!(code(&refused), 125, "{}", stderr(&refused));
    assert_eq!(last_record(&bare)["stage"], STAGE_PRESET);

    // A path the import could not rewrite byte for byte.
    let quoted = place.dir("qu\"oted");
    assert_eq!(code(&place.new_with_binary(&quoted, &script, &[])), 0);
    let quoted = quoted.canonicalize().unwrap();
    let refused = place.tool(&["freeze", &quoted.display().to_string()]);
    assert_eq!(code(&refused), 125, "{}", stderr(&refused));
    assert_eq!(last_record(&quoted)["stage"], STAGE_PATHS);

    // A sandbox marked frozen refuses every command, writing nothing.
    forge_frozen(&bare);
    chmod_a_minus_w(&bare.join("golden"));
    let before = records(&bare).len();
    let dir = bare.display().to_string();
    let ws = place.ws();
    let script_s = script.display().to_string();
    let commands: Vec<Vec<&str>> = vec![
        vec!["exec", &dir, "--", "--version"],
        vec!["restore", &dir],
        vec!["build", &dir],
        vec![
            "new",
            &dir,
            "--replace",
            "--preset",
            "bare",
            "--binary",
            &script_s,
            "--workspace",
            &ws,
        ],
        vec![
            "new",
            &dir,
            "--preset",
            "bare",
            "--binary",
            &script_s,
            "--workspace",
            &ws,
        ],
        vec!["check", &dir],
        vec!["stub", "start", &dir],
        vec!["daemon", "start", &dir],
    ];
    for args in commands {
        let out = place.tool(&args);
        assert_eq!(code(&out), 125, "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("the sandbox is frozen"),
            "{args:?}: {}",
            stderr(&out)
        );
        assert_eq!(
            records(&bare).len(),
            before,
            "{args:?}: a record was written"
        );
    }

    // A freeze interrupted after the marker: the golden is still
    // writable, the record is there, and a second freeze finishes it.
    let half = place.dir("half");
    assert_eq!(code(&place.new_with_binary(&half, &script, &[])), 0);
    let half = half.canonicalize().unwrap();
    let golden_sig = signature(&half.join("golden"));
    let record = json!({
        "schema": 1, "version": "stand-in 1", "binary_sha256": "", "tool_version": "x",
        "dir": half.display().to_string(), "golden_sig": golden_sig,
        "manifest_schema": 1, "frozen_at": 0.0,
    });
    std::fs::write(
        half.join("release.json"),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .expect("release.json");
    forge_frozen(&half);
    assert!(writable(&half.join("golden/registry.toml")));
    let finished = place.tool(&["freeze", &half.display().to_string()]);
    assert_eq!(code(&finished), 0, "{}", stderr(&finished));
    assert!(!writable(&half.join("golden/registry.toml")));
    assert!(!writable(&half.join("golden")));
    assert!(!writable(&half.join("release.json")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_opened_before_the_freeze_is_refused_after_it() {
    let place = Place::new();
    let script = place.script("binary.sh", &knobs_script(&PLANNED));
    let dir = place.dir("sbx");
    assert_eq!(code(&place.new_with_binary(&dir, &script, &[])), 0);
    let dir = dir.canonicalize().unwrap();

    // Opened before the freeze.
    let mut earlier =
        Session::open(&dir, SessionOpts::default()).unwrap_or_else(|o| panic!("open: {o:?}"));
    let before = records(&dir).len();
    let golden_sig = signature(&dir.join("golden"));
    forge_frozen(&dir);
    let refused = earlier
        .exec(&["--version".into()], Default::default())
        .await
        .expect_err("an exec after the freeze is refused");
    assert_eq!(refused.stage, STAGE_PATHS, "{refused:?}");
    let refused = earlier
        .restore()
        .await
        .expect_err("a restore after the freeze is refused");
    assert_eq!(refused.stage, STAGE_PATHS, "{refused:?}");
    assert_eq!(records(&dir).len(), before, "no record was written");
    assert_eq!(signature(&dir.join("golden")), golden_sig);

    // Frozen while waiting for the lock.
    let dir2 = place.dir("sbx2");
    assert_eq!(code(&place.new_with_binary(&dir2, &script, &[])), 0);
    let dir2 = dir2.canonicalize().unwrap();
    let mut waiting = Session::open(
        &dir2,
        SessionOpts {
            lock_wait: Duration::from_secs(10),
            pause_at: None,
        },
    )
    .unwrap_or_else(|o| panic!("open: {o:?}"));
    let before = records(&dir2).len();
    let lock = File::options()
        .read(true)
        .write(true)
        .open(dir2.join(".session.lock"))
        .expect("lock file");
    lock.lock().expect("hold the session lock");
    let version: [std::ffi::OsString; 1] = ["--version".into()];
    let (refused, ()) = tokio::join!(waiting.exec(&version, Default::default()), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        forge_frozen(&dir2);
        lock.unlock().expect("release the session lock");
    });
    let refused = refused.expect_err("the call that waited is refused once it holds the lock");
    assert_eq!(refused.stage, STAGE_PATHS, "{refused:?}");
    assert_eq!(records(&dir2).len(), before, "no record was written");
}
