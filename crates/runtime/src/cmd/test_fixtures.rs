// SPDX-License-Identifier: Apache-2.0

//! Fixtures shared by the command modules' tests.

use std::path::Path;

use bookrack_config::{Config, DaemonConfig, DaemonFlags, EnvSnapshot, OLLAMA_URL_ENV};

/// A `Config` over a fresh, empty data root.
pub(crate) fn temp_cfg() -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = Config::new(dir.path().to_path_buf());
    (dir, cfg)
}

/// A process-level configuration whose embed endpoint is `ollama_url`
/// and that serves no MCP listener, resolved from nothing else.
pub(crate) fn daemon_at(ollama_url: &str) -> DaemonConfig {
    daemon_from([(OLLAMA_URL_ENV, ollama_url)])
}

/// A process-level configuration resolved from exactly `pairs`, serving
/// no MCP listener.
pub(crate) fn daemon_from<const N: usize>(pairs: [(&str, &str); N]) -> DaemonConfig {
    DaemonConfig::resolve(
        &EnvSnapshot::from_pairs(pairs),
        &DaemonFlags {
            no_mcp: true,
            ..DaemonFlags::default()
        },
    )
}

/// [`daemon_at`] an endpoint nothing listens on, for a test that must
/// not reach a backend.
pub(crate) fn test_daemon() -> DaemonConfig {
    daemon_at(bookrack_config::UNROUTED_OLLAMA_URL)
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
