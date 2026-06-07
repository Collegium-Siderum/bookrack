// SPDX-License-Identifier: Apache-2.0

//! `bookrack info` — summarise the active data root, embedder, and
//! per-store stamps.

use anyhow::{Context, Result};
use bookrack_config::{Config, EmbedConfig};
use bookrack_ops::reads::info::LibraryInfoContext;

use crate::ops_helpers::catalog_only_ops;
use crate::render;
use crate::util::{resolution_source_label, static_source_label};

pub async fn run(cfg: &Config) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
    let ops = catalog_only_ops(cfg);
    let ctx = LibraryInfoContext {
        data_dir: cfg.data_dir().display().to_string(),
        library_name: cfg.library().map(str::to_string),
        resolution_source: resolution_source_label(cfg.source()).to_string(),
        ollama_url: cfg.ollama_url().to_string(),
        embed_model_configured: embed_cfg.model.clone(),
    };
    let info = bookrack_ops::reads::info::show_library_info(&ops, ctx)
        .await
        .context("read library info")?;
    render::info(&info_snapshot_from_ops(info));
    Ok(())
}

/// Adapt the ops DTO into the snapshot the CLI renderer prints. The
/// two shapes differ only in field naming and in `source` being a
/// string here, so the conversion stays inline rather than mint a
/// trait surface neither caller wants.
fn info_snapshot_from_ops(info: bookrack_ops::dto::info::LibraryInfo) -> render::InfoSnapshot {
    render::InfoSnapshot {
        data_dir: info.data_dir,
        library: info.library_name,
        source: static_source_label(info.resolution_source.as_str()),
        ollama_url: info.ollama_url,
        embed_model_configured: info.embed_model_configured,
        corpus_schema_version_expected: info.corpus_schema_version_expected,
        catalog_schema_version_expected: info.catalog_schema_version_expected,
        catalog_schema_version_on_disk: info.catalog_schema_version_on_disk,
        corpus_stamps: render::CorpusStamps {
            embed_model: info.corpus_stamps.embed_model,
            vector_dim: info.corpus_stamps.vector_dim,
            chunk_version: info.corpus_stamps.chunk_version,
            normalize_version: info.corpus_stamps.normalize_version,
            schema_version_on_disk: info.corpus_stamps.schema_version_on_disk,
        },
        vectors_meta: info.vectors_meta,
        current_chunks: info.current_chunks,
        intake_count: info.intake_count,
        ready_book_count: info.ready_book_count,
        disk: render::DiskUsage {
            catalog_db: info.disk.catalog_db,
            corpus_db: info.disk.corpus_db,
            lancedb_dir: info.disk.lancedb_dir,
        },
    }
}
