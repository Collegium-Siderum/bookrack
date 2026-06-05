// SPDX-License-Identifier: Apache-2.0

//! Read the four behaviour-sensitive stamps from `corpus.db`'s
//! `index_meta` table plus the on-disk schema version, and write them
//! to `<bundle>/corpus/index-meta.json`.

use std::path::Path;

use bookrack_config::Config;
use bookrack_corpus::{
    CHUNK_VERSION_KEY, Corpus, EMBED_MODEL_KEY, NORMALIZE_VERSION_KEY, VECTOR_DIM_KEY,
};

use crate::Result;

/// Write `<bundle>/corpus/index-meta.json`. A corpus.db that fails to
/// open is reported as a missing file: the manifest still references
/// the directory, but the JSON itself is omitted.
pub fn collect(cfg: &Config, bundle_dir: &Path) -> Result<()> {
    let dst = bundle_dir.join("corpus");
    std::fs::create_dir_all(&dst)?;

    let corpus = match Corpus::open(&cfg.corpus_db()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "diagnose: could not open corpus");
            return Ok(());
        }
    };
    let payload = serde_json::json!({
        "embed_model": corpus.meta_get(EMBED_MODEL_KEY).ok().flatten(),
        "vector_dim": corpus.meta_get(VECTOR_DIM_KEY).ok().flatten(),
        "chunk_version": corpus.meta_get(CHUNK_VERSION_KEY).ok().flatten(),
        "normalize_version": corpus.meta_get(NORMALIZE_VERSION_KEY).ok().flatten(),
        "schema_version_on_disk": corpus.meta_get("schema_version").ok().flatten(),
    });
    let mut text = serde_json::to_string_pretty(&payload)?;
    text.push('\n');
    std::fs::write(dst.join("index-meta.json"), text)?;
    Ok(())
}
