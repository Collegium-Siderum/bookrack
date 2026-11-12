// SPDX-License-Identifier: Apache-2.0

//! Fixtures shared by the command modules' tests.

use std::path::Path;

use bookrack_config::Config;

/// A `Config` over a fresh, empty data root.
pub(crate) fn temp_cfg() -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = Config::new(
        dir.path().to_path_buf(),
        "http://localhost:11434".to_string(),
    );
    (dir, cfg)
}

/// The stores sitting directly under `root`, sorted: every `*.db` file
/// and every `lancedb*` directory. SQLite's `-shm` / `-wal` sidecars
/// are left out, since a read-only connection to a WAL database
/// creates them.
pub(crate) fn store_files(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".db") || name.starts_with("lancedb"))
        .collect();
    names.sort();
    names
}
