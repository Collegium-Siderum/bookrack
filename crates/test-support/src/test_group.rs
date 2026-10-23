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

/// Declares the layout test for the group `main.rs` it is written in.
///
/// `$group` is the directory name under `tests/`; `$spec` is the
/// `Option<&str>` [`check_group`] takes.
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
    };
}
