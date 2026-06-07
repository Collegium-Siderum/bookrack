// SPDX-License-Identifier: Apache-2.0

//! `Ops` construction for short-lived CLI invocations that only browse
//! the catalog and corpus.

use bookrack_config::Config;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::{Caller, Ops};

/// Build a catalog-only [`Ops`] for short-lived CLI invocations that do
/// not need vector search. Skips the Ollama dimension probe so the
/// process can serve a `books *` subcommand in milliseconds.
pub fn catalog_only_ops(cfg: &Config) -> Ops<OllamaEmbedClient> {
    Ops::catalog_only(
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        cfg.books_dir(),
        cfg.backup_dir(),
        Caller::cli(),
    )
}
