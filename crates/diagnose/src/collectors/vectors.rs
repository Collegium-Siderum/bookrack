// SPDX-License-Identifier: Apache-2.0

//! Snapshot the vectors sidecar (`vectors_meta.json`) verbatim.

use std::path::Path;

use bookrack_config::Config;

use crate::Result;

/// Write `<bundle>/vectors/vectors_meta.json` if the sidecar exists.
/// A sidecar that exists but fails to load writes `open-error.json`
/// instead of silently vanishing from the bundle; an absent sidecar
/// is a normal state (fresh or legacy store) and writes nothing. The
/// lancedb-itself live state (row counts, fragment list, …) is not
/// captured here: opening the store would require the same runtime
/// the daemon uses, and the sidecar already carries the settings a
/// maintainer needs to reproduce the build.
pub fn collect(cfg: &Config, bundle_dir: &Path) -> Result<()> {
    let dst = bundle_dir.join("vectors");
    std::fs::create_dir_all(&dst)?;
    let lancedb_dir = cfg.lancedb_dir();
    match bookrack_vectors::meta::load(&lancedb_dir) {
        Ok(Some(m)) => {
            let mut text = serde_json::to_string_pretty(&m)?;
            text.push('\n');
            std::fs::write(dst.join("vectors_meta.json"), text)?;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, "diagnose: could not load vectors_meta.json");
            super::write_open_error(
                &dst,
                &lancedb_dir.join("vectors_meta.json"),
                Some(&e.to_string()),
            )?;
        }
    }
    Ok(())
}
