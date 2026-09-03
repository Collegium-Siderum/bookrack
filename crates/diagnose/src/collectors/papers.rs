// SPDX-License-Identifier: Apache-2.0

//! The paper pipeline's three stores, mirrored under `<bundle>/papers/`
//! in the same shapes the book-side collectors write.

use std::path::Path;

use bookrack_config::Config;

use crate::Result;
use crate::scrub::Scrubber;

/// Write `<bundle>/papers/{catalog,corpus,vectors}/…` from the paper
/// catalog, corpus, and vector sidecar. Each section follows its
/// book-side counterpart exactly — the same payload files, the same
/// `open-error.json` for a store that is missing or fails to open —
/// so a library that has never ingested a paper records that state in
/// the bundle rather than leaving the section out.
pub fn collect(cfg: &Config, since_ts: &str, bundle_dir: &Path, scrubber: &Scrubber) -> Result<()> {
    let root = bundle_dir.join("papers");
    super::catalog::collect_into(
        &cfg.papers_catalog_db(),
        &root.join("catalog"),
        since_ts,
        scrubber,
    )?;
    super::corpus::collect_into(&cfg.papers_corpus_db(), &root.join("corpus"))?;
    super::vectors::collect_into(&cfg.papers_lancedb_dir(), &root.join("vectors"))
}
