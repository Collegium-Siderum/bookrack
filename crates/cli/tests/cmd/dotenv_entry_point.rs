// SPDX-License-Identifier: Apache-2.0

//! Where the binary reads `.env`, and how to stop it.
//!
//! The switch itself is a pure function with its own unit tests. What
//! no pure test can check is *who calls it*, which is the whole point
//! of moving the load out of the configuration library: these two run
//! the real binary and read the root it resolved.

#![cfg(unix)]

use std::process::{Output, Stdio};

use bookrack_test_support::{Sandbox, bookrack_cmd};
use eyre::Result;

/// Write a `.env` in the sandbox's working directory naming a data root
/// nothing else names, and return that root.
fn seed_dotenv(sandbox: &Sandbox) -> std::path::PathBuf {
    let root = sandbox.data_root("from-dotenv");
    std::fs::write(
        sandbox.cwd().join(".env"),
        format!("BOOKRACK_DATA_DIR={}\n", root.display()),
    )
    .expect("write .env");
    root
}

/// Run `bookrack doctor --json` with no data root of its own, so the
/// only thing that can supply one is the seeded `.env`, and return the
/// value of its `data root` row.
///
/// `doctor` is the reader because it reports the root it resolved
/// whether or not the resolution succeeded — the exit code tracks the
/// health of the probes, which an offline sandbox fails for unrelated
/// reasons.
async fn resolved_root(sandbox: &Sandbox, extra: &[(&str, &str)]) -> Result<String> {
    let mut spawn = bookrack_cmd!(sandbox).without_data_dir();
    for (key, value) in extra {
        spawn = spawn.extra_env(key, value);
    }
    let out: Output = tokio::process::Command::from(spawn.build())
        .args(["doctor", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        eyre::eyre!(
            "doctor --json did not print a report ({e}); stdout={stdout} stderr={}",
            String::from_utf8_lossy(&out.stderr),
        )
    })?;
    let row = report["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["label"] == "data root"))
        .ok_or_else(|| eyre::eyre!("no data-root row in {report}"))?;
    Ok(row["value"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("no value on {row}"))?
        .to_string())
}

/// The binary loads `.env` from its own working directory.
///
/// Green before the relocation as well as after — the library was doing
/// the loading — but it is the net under the relocation itself: an
/// entry point the move missed turns this red, and a missed entry point
/// is the one real hazard in moving a call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binary_loads_dotenv_from_its_working_directory() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = seed_dotenv(&sandbox);

    assert_eq!(
        resolved_root(&sandbox, &[]).await?,
        root.display().to_string(),
        "the root named in .env must be the one resolved",
    );
    Ok(())
}

/// The suppression switch works from the real environment, against a
/// `.env` that is right there and would otherwise be read.
///
/// This is what `scripts/test-clean.sh` stands on: `env -i` cannot
/// reach a file, and cargo runs every test binary from its package
/// root, so without this switch the repository's own `.env` refills a
/// scrubbed environment. With the file suppressed and the sandbox
/// registry empty, nothing is left to resolve from — so the run has to
/// report exactly that, rather than resolving anything at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_dotenv_makes_the_binary_ignore_a_present_dotenv() -> Result<()> {
    let sandbox = Sandbox::new();
    let root = seed_dotenv(&sandbox);

    let resolved = resolved_root(&sandbox, &[("BOOKRACK_NO_DOTENV", "1")]).await?;
    assert_ne!(
        resolved,
        root.display().to_string(),
        "the suppressed .env still reached the binary",
    );
    assert_eq!(
        resolved, "(none configured)",
        "with .env suppressed and an empty registry there is nothing left \
         to resolve from, so the run must say so rather than resolve \
         something else",
    );
    Ok(())
}
