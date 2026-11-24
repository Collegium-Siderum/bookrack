// SPDX-License-Identifier: Apache-2.0

//! A registry entry written by another build keeps what this build does
//! not know — a kind token, an extra key — through the commands that
//! rewrite the entry.

use std::process::Output;

use bookrack_test_support::{Sandbox, bookrack_cmd};
use tokio::process::Command;

async fn bookrack(sandbox: &Sandbox, args: &[&str]) -> Output {
    Command::from(bookrack_cmd!(sandbox).build())
        .args(args)
        .output()
        .await
        .expect("run bookrack")
}

#[tokio::test]
async fn clearing_the_cached_profile_removes_only_that_field() {
    let sandbox = Sandbox::new();
    let root = sandbox.data_root("lib-root");
    // The entry as another build writes it: a kind token this build
    // does not define, a cached profile, and a key it does not know.
    // JSON string escaping is a subset of TOML's, so the encoder quotes
    // the root as a valid basic string.
    let registry = sandbox.write_registry(&format!(
        "default = \"lib\"\n\n[libraries.lib]\ndata_dir = {}\nkind = \"archive\"\n\
         index_profile = \"cached\"\nmount_at_start = false\n",
        serde_json::to_string(&root.display().to_string()).expect("quote the root"),
    ));

    let listing = bookrack(&sandbox, &["--json", "libraries", "list"]).await;
    let stdout = String::from_utf8_lossy(&listing.stdout);
    assert!(
        listing.status.success() && stdout.contains("\"kind\":\"archive\""),
        "an unknown kind lists as its token: {stdout} {}",
        String::from_utf8_lossy(&listing.stderr),
    );

    let unset = bookrack(
        &sandbox,
        &["libraries", "config", "lib", "--unset", "index_profile"],
    )
    .await;
    assert!(
        unset.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&unset.stderr)
    );

    let after = std::fs::read_to_string(&registry).expect("read registry");
    assert!(
        !after.contains("index_profile"),
        "the cached profile is cleared: {after}"
    );
    assert!(
        after.contains("kind = \"archive\""),
        "the kind is kept: {after}"
    );
    assert!(
        after.contains("mount_at_start = false"),
        "a key this build does not know is kept: {after}"
    );
}
