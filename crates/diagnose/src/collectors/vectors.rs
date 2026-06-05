// SPDX-License-Identifier: Apache-2.0

//! Snapshot the vectors sidecar (`vectors_meta.json`) verbatim.

use std::path::Path;

use bookrack_config::Config;

use crate::Result;

/// Write `<bundle>/vectors/vectors_meta.json` if the sidecar exists.
/// The lancedb-itself live state (row counts, fragment list, …) is
/// not captured here: opening the store would require the same
/// runtime the daemon uses, and the sidecar already carries the
/// settings a maintainer needs to reproduce the build.
pub fn collect(cfg: &Config, bundle_dir: &Path) -> Result<()> {
    let dst = bundle_dir.join("vectors");
    std::fs::create_dir_all(&dst)?;
    let lancedb_dir = cfg.lancedb_dir();
    let meta = bookrack_vectors::meta::load(&lancedb_dir).ok().flatten();
    if let Some(m) = meta {
        let mut text = serde_json::to_string_pretty(&m)?;
        text.push('\n');
        std::fs::write(dst.join("vectors_meta.json"), text)?;
    }
    Ok(())
}
