// SPDX-License-Identifier: Apache-2.0

//! The layout guard for a grouped integration test binary.
//!
//! Cargo builds every `tests/<name>.rs` as a binary of its own, and each
//! binary links the whole dependency tree. A crate whose suite is large
//! groups it instead: one `tests/<group>/main.rs` per process-environment
//! spec, declaring each sibling file as a module. Two things then go
//! unnoticed by every tool in the chain, and this module pins both:
//!
//! - a sibling file that `main.rs` does not declare is never compiled,
//!   not even to the point of a `compile_error!`;
//! - a file that installs a different [`crate::ProcessEnv`] than the
//!   rest of its group panics under `cargo test`, where one binary is
//!   one process, but passes under `cargo nextest`, where it is not.
//!
//! [`check_group`] reads the group directory as text, because the
//! property it checks is a property of the text: which file sits in
//! which directory, and what that file writes. [`test_group!`] puts it
//! in a group's `main.rs` as a test.
//!
//! A group that installs a process environment also shares it, and the
//! embed stub whose URL it carries, across every test in the binary.
//! Those tests collide with each other unless each has a process of its
//! own, so [`check_execution_mode`] refuses any other runner in one
//! sentence that names the remedy; [`test_group!`] emits that check as
//! a second test, in place of the batch of unrelated-looking failures
//! the collisions would otherwise produce.

use std::fs;
use std::path::Path;

/// Verdict on one grouped integration test directory.
///
/// `group_dir` is `tests/<group>` under a crate manifest. `spec` is the
/// one `ProcessEnv` expression every file in the group may hand to
/// `process_env`, written exactly as the files write it, or `None` for
/// a group whose files install nothing. The `Err` names every
/// violation, one per line.
pub fn check_group(group_dir: &Path, spec: Option<&str>) -> Result<(), String> {
    let main = group_dir.join("main.rs");
    let main_src = fs::read_to_string(&main)
        .map_err(|err| format!("{}: cannot read main.rs: {err}", group_dir.display()))?;
    let declared = declared_modules(&main_src);
    let siblings = sibling_stems(group_dir)?;
    let mut why = Vec::new();

    for stem in &siblings {
        if !declared.iter().any(|(name, _)| name == stem) {
            why.push(format!(
                "{}: {stem}.rs is not declared in main.rs",
                group_dir.display()
            ));
        }
    }
    for (name, has_path) in &declared {
        if !has_path && !siblings.contains(name) {
            why.push(format!(
                "{}: main.rs declares `mod {name};` but there is no {name}.rs beside it",
                group_dir.display()
            ));
        }
    }
    for stem in &siblings {
        let file = group_dir.join(format!("{stem}.rs"));
        let src = fs::read_to_string(&file)
            .map_err(|err| format!("{}: cannot read: {err}", file.display()))?;
        for (index, line) in src.lines().enumerate() {
            if !line.contains("process_env(") {
                continue;
            }
            let number = index + 1;
            match spec {
                Some(spec) if line.contains(&format!("process_env({spec})")) => {}
                Some(_) => why.push(format!(
                    "{}:{number}: installs a different process environment than this group",
                    file.display()
                )),
                None => why.push(format!(
                    "{}:{number}: installs a process environment, and this group installs none",
                    file.display()
                )),
            }
        }
    }

    if why.is_empty() {
        Ok(())
    } else {
        Err(why.join("\n"))
    }
}

/// Every `mod <name>;` in `main.rs`, with whether a `#[path = ...]`
/// attribute precedes it. A `#[cfg(...)]` on the same or the previous
/// line is skipped over; nothing else about the declaration is parsed.
fn declared_modules(main_src: &str) -> Vec<(String, bool)> {
    let mut found = Vec::new();
    let mut path_pending = false;
    for raw in main_src.lines() {
        let mut line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#[path") {
            path_pending = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("#[cfg(") {
            line = match rest.find(")]") {
                Some(end) => rest[end + 2..].trim(),
                None => continue,
            };
            if line.is_empty() {
                continue;
            }
        }
        if let Some(name) = line
            .strip_prefix("mod ")
            .and_then(|rest| rest.strip_suffix(';'))
        {
            found.push((name.trim().to_string(), path_pending));
        }
        path_pending = false;
    }
    found
}

/// The stems of the `.rs` files beside `main.rs`, sorted. Directories
/// are not listed: a `common/` beside the modules is reached through
/// `#[path]`, never declared as a sibling.
fn sibling_stems(group_dir: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(group_dir)
        .map_err(|err| format!("{}: cannot list: {err}", group_dir.display()))?;
    let mut stems = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("{}: cannot list: {err}", group_dir.display()))?;
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if stem != "main" {
            stems.push(stem.to_string());
        }
    }
    stems.sort();
    Ok(stems)
}

/// The variable `cargo nextest` sets in every test process it starts.
pub const NEXTEST_EXECUTION_MODE_ENV: &str = "NEXTEST_EXECUTION_MODE";

/// The [`NEXTEST_EXECUTION_MODE_ENV`] value under which each test runs
/// in a process of its own.
pub const PROCESS_PER_TEST: &str = "process-per-test";

/// Verdict on the runner a grouped binary was started by.
///
/// `installs_process_env` is whether the group hands a spec to
/// `process_env`; a group that installs nothing shares nothing through
/// the environment and runs under any runner. `mode` is the value of
/// [`NEXTEST_EXECUTION_MODE_ENV`] in the test process, `None` when the
/// variable is absent. The `Err` is one sentence naming the remedy.
pub fn check_execution_mode(installs_process_env: bool, mode: Option<&str>) -> Result<(), String> {
    if !installs_process_env {
        return Ok(());
    }
    let seen = match mode {
        Some(PROCESS_PER_TEST) => return Ok(()),
        Some(other) => format!("{NEXTEST_EXECUTION_MODE_ENV} is {other:?}"),
        None => format!("{NEXTEST_EXECUTION_MODE_ENV} is not set"),
    };
    Err(format!(
        "this binary installs one process environment for every test in it and needs one \
         process per test; run it under cargo nextest ({seen}, not {PROCESS_PER_TEST:?})"
    ))
}

/// Declares the guard tests for the group `main.rs` it is written in.
///
/// `$group` is the directory name under `tests/`; `$spec` is the
/// `Option<&str>` [`check_group`] takes. The layout test runs under
/// any runner; the execution-mode test is a one-sentence refusal of a
/// runner that gives a process-environment group one process for the
/// whole binary.
#[macro_export]
macro_rules! test_group {
    ($group:literal, spec = $spec:expr) => {
        #[test]
        fn integration_test_group_layout() {
            let dir = ::std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join($group);
            if let Err(why) = $crate::test_group::check_group(&dir, $spec) {
                panic!("{why}");
            }
        }

        #[test]
        fn integration_test_group_runs_one_process_per_test() {
            let spec: ::std::option::Option<&str> = $spec;
            let mode = ::std::env::var($crate::test_group::NEXTEST_EXECUTION_MODE_ENV).ok();
            if let Err(why) =
                $crate::test_group::check_execution_mode(spec.is_some(), mode.as_deref())
            {
                panic!("{why}");
            }
        }
    };
}
