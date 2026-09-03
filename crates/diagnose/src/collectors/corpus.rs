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

/// Write `<bundle>/corpus/index-meta.json` from the book corpus. See
/// [`collect_into`] for what a missing or unopenable store writes
/// instead.
pub fn collect(cfg: &Config, bundle_dir: &Path) -> Result<()> {
    collect_into(&cfg.corpus_db(), &bundle_dir.join("corpus"))
}

/// Read the stamps of the corpus at `corpus_db` into
/// `<dst>/index-meta.json`. The corpus is opened through the read-only
/// door, so collecting neither materialises a missing corpus.db nor
/// takes the write lock. A corpus.db that is missing or fails to open
/// writes `open-error.json` in place of the payload, keeping the two
/// states distinguishable in the bundle. The book and paper corpora
/// share one schema, so both sides route through here.
pub(crate) fn collect_into(corpus_db: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;

    if !corpus_db.exists() {
        return super::write_open_error(dst, corpus_db, None);
    }
    let corpus = match Corpus::open_read_only(corpus_db) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "diagnose: could not open corpus read-only");
            return super::write_open_error(dst, corpus_db, Some(&e.to_string()));
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
