// SPDX-License-Identifier: Apache-2.0

//! `check_group` against synthetic group directories.

use std::fs;
use std::path::{Path, PathBuf};

use bookrack_test_support::test_group::check_group;
use tempfile::TempDir;

/// A `tests/` tree with one group directory `g` and a `common/mod.rs`
/// beside it, so a `#[path = "../common/mod.rs"]` declaration resolves.
fn tests_tree(files: &[(&str, &str)]) -> (TempDir, PathBuf) {
    let tree = tempfile::tempdir().expect("tempdir");
    let group = tree.path().join("g");
    fs::create_dir_all(tree.path().join("common")).expect("common dir");
    fs::write(tree.path().join("common/mod.rs"), "pub fn helper() {}\n").expect("common");
    fs::create_dir_all(&group).expect("group dir");
    for (name, body) in files {
        fs::write(group.join(name), body).expect(name);
    }
    (tree, group)
}

fn failure(group: &Path, spec: Option<&str>) -> String {
    check_group(group, spec).expect_err("the group must be refused")
}

#[test]
fn an_undeclared_sibling_is_named() {
    let (_tree, group) = tests_tree(&[
        ("main.rs", "mod a;\n"),
        ("a.rs", "#[test] fn a() {}\n"),
        ("b.rs", "#[test] fn b() {}\n"),
    ]);
    let why = failure(&group, None);
    assert!(
        why.contains("b.rs") && why.contains("not declared"),
        "{why}"
    );
    assert!(!why.contains("a.rs"), "{why}");
}

#[test]
fn a_foreign_spec_is_named_with_its_line() {
    let (_tree, group) = tests_tree(&[
        ("main.rs", "mod a;\n"),
        (
            "a.rs",
            "use bookrack_test_support::{ProcessEnv, process_env};\n\
             #[test] fn a() {\n    process_env(ProcessEnv::isolated());\n}\n",
        ),
    ]);
    let why = failure(&group, Some("ProcessEnv::daemon()"));
    assert!(why.contains("a.rs:3"), "{why}");
    assert!(why.contains("different process environment"), "{why}");
}

#[test]
fn a_spec_in_a_specless_group_is_named() {
    let (_tree, group) = tests_tree(&[
        ("main.rs", "mod a;\n"),
        (
            "a.rs",
            "#[test] fn a() {\n    let _ = process_env(ProcessEnv::daemon());\n}\n",
        ),
    ]);
    let why = failure(&group, None);
    assert!(why.contains("a.rs:2"), "{why}");
    assert!(why.contains("installs none"), "{why}");
}

#[test]
fn a_declaration_without_a_sibling_is_named() {
    let (_tree, group) = tests_tree(&[("main.rs", "mod a;\nmod gone;\n"), ("a.rs", "")]);
    let why = failure(&group, None);
    assert!(why.contains("gone") && why.contains("no gone.rs"), "{why}");
}

#[test]
fn a_well_formed_group_passes() {
    let (_tree, group) = tests_tree(&[
        (
            "main.rs",
            "#[path = \"../common/mod.rs\"]\nmod common;\n\n\
             bookrack_test_support::test_group!(\"g\", spec = Some(\"ProcessEnv::daemon()\"));\n\n\
             mod a;\n#[cfg(unix)]\nmod c;\n#[cfg(windows)] mod w;\n",
        ),
        (
            "a.rs",
            "#[test] fn a() {\n    process_env(ProcessEnv::daemon());\n}\n",
        ),
        (
            "c.rs",
            "#![cfg(unix)]\nlet sandbox = process_env(ProcessEnv::daemon());\n",
        ),
        ("w.rs", ""),
    ]);
    check_group(&group, Some("ProcessEnv::daemon()")).expect("a well-formed group passes");
}

#[test]
fn a_missing_main_is_named() {
    let (_tree, group) = tests_tree(&[("a.rs", "")]);
    let why = failure(&group, None);
    assert!(why.contains("main.rs"), "{why}");
}
