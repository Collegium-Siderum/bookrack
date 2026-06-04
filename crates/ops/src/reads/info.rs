// SPDX-License-Identifier: Apache-2.0

//! Library-status reads.
//!
//! The on-disk facts — corpus stamps, vectors metadata, catalog counts,
//! file sizes — live in this crate. The static facts about where the
//! library is open and how it was configured ride in on
//! [`LibraryInfoContext`], which the caller fills from its
//! [`bookrack_config::Config`].

use std::path::Path;

use bookrack_catalog::Catalog;
use bookrack_corpus::{
    CHUNK_VERSION_KEY, Corpus, EMBED_MODEL_KEY, NORMALIZE_VERSION_KEY, VECTOR_DIM_KEY,
};
use bookrack_embed::Embedder;
use bookrack_vectors::ChunkStore;

use crate::Ops;
use crate::Result;
use crate::dto::info::{CorpusStamps, DiskUsage, LibraryInfo};

/// Static facts about the library being inspected. The caller fills
/// this from its [`bookrack_config::Config`] before calling
/// [`show_library_info`].
#[derive(Debug, Clone)]
pub struct LibraryInfoContext {
    /// Where the library lives on disk (rendered).
    pub data_dir: String,
    /// Registry name of the open library, when one was selected.
    pub library_name: Option<String>,
    /// How the data-dir was resolved.
    pub resolution_source: String,
    /// Ollama HTTP endpoint the daemon will reach.
    pub ollama_url: String,
    /// Model tag the daemon is configured to embed with.
    pub embed_model_configured: String,
}

/// Read the one-page library status card.
///
/// Errors are swallowed for the live values (corpus stamps, vectors
/// meta, chunk count, intake counts, disk sizes) so this stays
/// informational rather than failing on a half-built library.
pub async fn show_library_info<E: Embedder>(
    ops: &Ops<E>,
    ctx: LibraryInfoContext,
) -> Result<LibraryInfo> {
    let corpus_stamps = read_corpus_stamps(ops.corpus_db()).unwrap_or_default();
    let vectors_meta = bookrack_vectors::meta::load(ops.lancedb_dir())
        .ok()
        .flatten();
    let current_chunks = read_current_chunk_count(ops.lancedb_dir(), &corpus_stamps).await;
    let intake_count = Catalog::open_read_only(ops.catalog_db())
        .and_then(|c| c.count_intakes())
        .ok();
    let ready_book_count = Catalog::open_read_only(ops.catalog_db())
        .and_then(|c| c.count_book_states_by_stage("ready"))
        .ok();
    Ok(LibraryInfo {
        data_dir: ctx.data_dir,
        library_name: ctx.library_name,
        resolution_source: ctx.resolution_source,
        ollama_url: ctx.ollama_url,
        embed_model_configured: ctx.embed_model_configured,
        corpus_schema_version_expected: bookrack_corpus::SCHEMA_VERSION,
        catalog_schema_version_expected: bookrack_catalog::SCHEMA_VERSION,
        corpus_stamps,
        vectors_meta,
        current_chunks,
        intake_count,
        ready_book_count,
        disk: disk_usage(ops.catalog_db(), ops.corpus_db(), ops.lancedb_dir()),
    })
}

fn read_corpus_stamps(corpus_db: &Path) -> Result<CorpusStamps> {
    let corpus = Corpus::open(corpus_db)?;
    Ok(CorpusStamps {
        embed_model: corpus.meta_get(EMBED_MODEL_KEY).ok().flatten(),
        vector_dim: corpus.meta_get(VECTOR_DIM_KEY).ok().flatten(),
        chunk_version: corpus.meta_get(CHUNK_VERSION_KEY).ok().flatten(),
        normalize_version: corpus.meta_get(NORMALIZE_VERSION_KEY).ok().flatten(),
        schema_version_on_disk: corpus.meta_get("schema_version").ok().flatten(),
    })
}

async fn read_current_chunk_count(lancedb_dir: &Path, stamps: &CorpusStamps) -> Option<usize> {
    let dim: usize = stamps.vector_dim.as_deref()?.parse().ok()?;
    let store = ChunkStore::open(lancedb_dir, dim).await.ok()?;
    store.count_rows().await.ok()
}

fn disk_usage(catalog_db: &Path, corpus_db: &Path, lancedb_dir: &Path) -> DiskUsage {
    DiskUsage {
        catalog_db: file_size(catalog_db),
        corpus_db: file_size(corpus_db),
        lancedb_dir: dir_size(lancedb_dir),
    }
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

fn dir_size(path: &Path) -> Option<u64> {
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let read_dir = std::fs::read_dir(&p).ok()?;
        for entry in read_dir.flatten() {
            let meta = entry.metadata().ok()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Some(total)
}
