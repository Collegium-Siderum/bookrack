// SPDX-License-Identifier: Apache-2.0

//! Embedding client construction. Wraps `OllamaEmbedClient::new` so
//! every `cmd/*` module that needs an embedder builds it the same way
//! from the same `Config` + `EmbedConfig` pair.

use anyhow::{Context, Result};
use bookrack_config::{Config, EmbedConfig};
use bookrack_embed::OllamaEmbedClient;

/// Build the embedding client from the environment-resolved knobs.
pub fn embedder(cfg: &Config, embed_cfg: &EmbedConfig) -> Result<OllamaEmbedClient> {
    OllamaEmbedClient::new(
        cfg.ollama_url(),
        &embed_cfg.model,
        embed_cfg.request_timeout,
        embed_cfg.max_retries,
        embed_cfg.backoff_base,
    )
    .context("build embedding client")
}
