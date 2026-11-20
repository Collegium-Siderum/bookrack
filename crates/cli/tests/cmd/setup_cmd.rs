// SPDX-License-Identifier: Apache-2.0

//! `bookrack setup` and the two deprecated `doctor` install flags that
//! now call it. Nothing here reaches the network: where an install is
//! exercised, the library is already in the sandbox's managed
//! directory, so the installer finds it and downloads nothing.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Output;

use bookrack_test_support::{Sandbox, bookrack_cmd};
use tokio::process::Command;

use super::source_scan::{member_crates, rust_sources, test_module_start, workspace_root};

async fn bookrack(sandbox: &Sandbox, args: &[&str]) -> Output {
    Command::from(bookrack_cmd!(sandbox).build())
        .args(args)
        .output()
        .await
        .expect("run bookrack")
}

/// Put a stand-in library where this build's managed PDFium lives in
/// the sandbox, and return its path.
fn seed_pdfium(sandbox: &Sandbox) -> PathBuf {
    let mut dir = sandbox.managed_data_dir().join("bookrack").join("pdfium");
    for segment in bookrack_ops::PDFIUM_RELEASE_TAG.split('/') {
        dir.push(segment);
    }
    std::fs::create_dir_all(&dir).expect("create the managed directory");
    let library = dir.join(bookrack_config::pdfium_library_filename());
    std::fs::write(&library, b"stand-in").expect("write the stand-in library");
    library
}

#[tokio::test]
async fn setup_without_a_component_lists_both_and_exits_zero() {
    let sandbox = Sandbox::new();

    let out = bookrack(&sandbox, &["--json", "setup"]).await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listing: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("setup --json prints JSON");
    let components = listing["components"].as_array().expect("components");
    let named = |prefix: &str| {
        components.iter().any(|component| {
            component["component"]
                .as_str()
                .is_some_and(|name| name.starts_with(prefix))
        })
    };
    assert!(named("pdfium") && named("reranker"), "{listing}");
    let pdfium = components
        .iter()
        .find(|component| component["component"] == "pdfium")
        .expect("pdfium");
    assert_eq!(
        pdfium["installed"], false,
        "a fresh sandbox holds none: {pdfium}"
    );
    assert!(pdfium["path"].is_null(), "{pdfium}");

    let library = seed_pdfium(&sandbox);
    let out = bookrack(&sandbox, &["--json", "setup"]).await;
    let listing: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("setup --json prints JSON");
    let pdfium = listing["components"]
        .as_array()
        .and_then(|components| components.iter().find(|c| c["component"] == "pdfium"))
        .expect("pdfium");
    assert_eq!(pdfium["installed"], true, "{pdfium}");
    assert_eq!(pdfium["path"], library.display().to_string());

    let human = bookrack(&sandbox, &["setup"]).await;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("pdfium") && text.contains("installed") && text.contains("setup reranker"),
        "{text}"
    );
}

#[tokio::test]
async fn setup_pdfium_leaves_an_installed_library_alone() {
    let sandbox = Sandbox::new();
    let library = seed_pdfium(&sandbox);

    let out = bookrack(&sandbox, &["setup", "pdfium"]).await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&format!("{} already installed", library.display())),
        "{text}"
    );
    assert_eq!(std::fs::read(&library).expect("read"), b"stand-in");
}

#[tokio::test]
async fn setup_pdfium_from_an_archive_with_another_digest_is_refused() {
    let sandbox = Sandbox::new();
    let archive = sandbox.inputs().join("not-the-pinned-archive.tgz");
    std::fs::create_dir_all(sandbox.inputs()).expect("inputs");
    std::fs::write(&archive, b"not a release archive").expect("write");

    let out = bookrack(
        &sandbox,
        &["setup", "pdfium", "--from", &archive.display().to_string()],
    )
    .await;
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("SHA-256 mismatch"), "{stderr}");
    assert!(
        !sandbox
            .managed_data_dir()
            .join("bookrack")
            .join("pdfium")
            .exists(),
        "a refused archive left something in the managed directory"
    );
}

#[tokio::test]
async fn the_doctor_install_flags_still_work_and_say_they_are_deprecated() {
    let sandbox = Sandbox::new();
    let library = seed_pdfium(&sandbox);

    let out = bookrack(&sandbox, &["doctor", "--install-pdfium"]).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let warnings: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("warning: "))
        .collect();
    assert_eq!(warnings.len(), 1, "{stderr}");
    assert!(
        warnings[0].contains("`bookrack doctor --install-pdfium` is deprecated")
            && warnings[0].contains("`bookrack setup pdfium`"),
        "{stderr}"
    );
    // The install line, then the report the flag always preceded.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    assert_eq!(
        lines.next(),
        Some(format!("{} already installed", library.display()).as_str()),
        "{stdout}"
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("report by")),
        "the doctor report no longer follows the install: {stdout}"
    );

    // The exit code is the report's, as it is without the flag.
    let plain = bookrack(&sandbox, &["doctor"]).await;
    assert_eq!(out.status.code(), plain.status.code());
    assert!(
        !String::from_utf8_lossy(&plain.stderr).contains("deprecated"),
        "a plain doctor warns about nothing"
    );
}

/// Guidance points at `setup`. Outside the CLI's own deprecation
/// handling, no shipped source names a `doctor` install flag.
#[test]
fn no_guidance_names_a_doctor_install_flag() {
    let root = workspace_root();
    let mut offenders = Vec::new();
    for member in member_crates(&root).expect("list crates") {
        for file in rust_sources(&root.join("crates").join(&member)).expect("list sources") {
            if file.ends_with("cli/src/main.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).expect("read source");
            let lines: Vec<&str> = text.lines().collect();
            let shipped = test_module_start(&lines);
            for (idx, line) in lines[..shipped].iter().enumerate() {
                // A test asserting the flag is absent names it too.
                let asserts_absence = line.contains("!") && line.contains(".contains(\"--install-");
                if !asserts_absence
                    && (line.contains("--install-pdfium") || line.contains("--install-reranker"))
                {
                    offenders.push(format!("{}:{}", file.display(), idx + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these lines still send an operator to a deprecated flag: {offenders:?}"
    );
}
