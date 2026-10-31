// SPDX-License-Identifier: Apache-2.0

//! What a new sandbox is filled with before its first snapshot.

use std::io;

use crate::sandbox::Sandbox;

/// The index profile every sandbox carries beside its registry: the
/// default model with a one-partition IVF index, so a small library
/// builds an index at all.
pub const PROFILE_SANDBOX_FLAT: &str = r#"schema_version = 1
name = "sandbox-flat"
description = "Sandbox profile: the default model with a one-partition IVF index."

[embed]
backend = "ollama"
model = "qwen3-embedding:0.6b"
dim = 1024

[ann]
kind = "ivf-flat"
num_partitions = 1
nprobes = 40

[reranker]
kind = "none"
"#;

/// The library the `bare` preset registers, as the default.
pub const BARE_LIBRARY: &str = "alpha";

/// Write the sandbox's index profile into `host/index-profiles/`.
pub fn write_profile(host: &Sandbox) -> io::Result<()> {
    let dir = host.path().join("index-profiles");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("sandbox-flat.toml"), PROFILE_SANDBOX_FLAT)
}

/// The `bare` preset: one empty root under `roots/`, registered as the
/// default library. The binary under test initialises the root the first
/// time it opens it; the tool only writes the registry, the same way the
/// integration suites do.
pub fn bare(host: &Sandbox) -> io::Result<()> {
    let root = host.data_root(BARE_LIBRARY);
    host.write_registry_entries(Some(BARE_LIBRARY), &[(BARE_LIBRARY, root.as_path())]);
    Ok(())
}
