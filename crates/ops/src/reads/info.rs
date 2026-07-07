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
use crate::dto::info::{CorpusStamps, DiskUsage, LibraryInfo, PapersInfo};
use crate::recorder::record_call_async;

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
    /// A registry `default` library eclipsed by a path-class
    /// resolution, pre-rendered for the card, or `None` when no default
    /// is silently overridden.
    pub shadowed_default: Option<String>,
    /// How the open library's name was identified against the registry
    /// for a path-class root, pre-rendered for the card, or `None` when
    /// the root is anonymous or the name came from a registry selection.
    pub library_identification: Option<String>,
    /// Ollama HTTP endpoint the daemon will reach.
    pub ollama_url: String,
    /// Model tag the daemon is configured to embed with.
    pub embed_model_configured: String,
    /// MCP listener address the daemon advertises through the lock
    /// file and `session.info`, in `host:port` form. Empty when the
    /// daemon is running without an MCP listener (e.g. `bookrack run
    /// --no-mcp`).
    pub mcp_addr: String,
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
    record_call_async!(ops, "library.info", serde_json::Value::Null, {
        let corpus_stamps = read_corpus_stamps(ops.corpus_db()).unwrap_or_default();
        let vectors_meta = bookrack_vectors::meta::load(ops.lancedb_dir())
            .ok()
            .flatten();
        let current_chunks = read_current_chunk_count(ops.lancedb_dir(), &corpus_stamps).await;
        // Open the catalog only when its file is on disk: rusqlite's
        // open creates the file on demand, which on a fresh data root
        // would materialise `catalog.db` just to read three numbers off it.
        let catalog = if ops.catalog_db().exists() {
            Catalog::open_read_only(ops.catalog_db()).ok()
        } else {
            None
        };
        let intake_count = catalog.as_ref().and_then(|c| c.count_intakes().ok());
        let ready_book_count = catalog
            .as_ref()
            .and_then(|c| c.count_book_states_by_stage("ready").ok());
        let catalog_schema_version_on_disk = catalog
            .as_ref()
            .and_then(|c| c.schema_version_on_disk().ok().flatten());
        let papers = read_papers_info(ops).await;
        Ok(LibraryInfo {
            data_dir: ctx.data_dir,
            library_name: ctx.library_name,
            resolution_source: ctx.resolution_source,
            shadowed_default: ctx.shadowed_default,
            library_identification: ctx.library_identification,
            ollama_url: ctx.ollama_url,
            embed_model_configured: ctx.embed_model_configured,
            corpus_schema_version_expected: bookrack_corpus::SCHEMA_VERSION,
            catalog_schema_version_expected: bookrack_catalog::SCHEMA_VERSION,
            catalog_schema_version_on_disk,
            corpus_stamps,
            vectors_meta,
            current_chunks,
            intake_count,
            ready_book_count,
            disk: disk_usage(ops.catalog_db(), ops.corpus_db(), ops.lancedb_dir()),
            papers,
        })
    })
}

/// Read the paper-side companion section, mirroring the book-side
/// reads above. Returns `None` when the calling `Ops` was built
/// without a papers backend; otherwise tolerates missing files for the
/// same reason the book-side path does (informational, not authoritative).
async fn read_papers_info<E: Embedder>(ops: &Ops<E>) -> Option<PapersInfo> {
    let corpus_db = ops.papers_corpus_db()?;
    let catalog_db = ops.papers_catalog_db()?;
    let lancedb_dir = ops.papers_lancedb_dir()?;
    let corpus_stamps = read_corpus_stamps(corpus_db).unwrap_or_default();
    let vectors_meta = bookrack_vectors::meta::load(lancedb_dir).ok().flatten();
    let current_chunks = read_current_chunk_count(lancedb_dir, &corpus_stamps).await;
    let catalog = if catalog_db.exists() {
        Catalog::open_read_only(catalog_db).ok()
    } else {
        None
    };
    let intake_count = catalog.as_ref().and_then(|c| c.count_intakes().ok());
    Some(PapersInfo {
        corpus_stamps,
        vectors_meta,
        current_chunks,
        intake_count,
        disk: disk_usage(catalog_db, corpus_db, lancedb_dir),
    })
}

fn read_corpus_stamps(corpus_db: &Path) -> Result<CorpusStamps> {
    if !corpus_db.exists() {
        return Ok(CorpusStamps::default());
    }
    let corpus = Corpus::open_read_only(corpus_db)?;
    Ok(CorpusStamps {
        embed_model: corpus.meta_get(EMBED_MODEL_KEY).ok().flatten(),
        vector_dim: corpus.meta_get(VECTOR_DIM_KEY).ok().flatten(),
        chunk_version: corpus.meta_get(CHUNK_VERSION_KEY).ok().flatten(),
        normalize_version: corpus.meta_get(NORMALIZE_VERSION_KEY).ok().flatten(),
        schema_version_on_disk: corpus.meta_get("schema_version").ok().flatten(),
    })
}

async fn read_current_chunk_count(lancedb_dir: &Path, _stamps: &CorpusStamps) -> Option<usize> {
    let store = ChunkStore::try_open(lancedb_dir).await.ok()??;
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
    // Top-level absence «no lancedb yet» surfaces as `None` so the
    // caller's DTO treats it as "unknown / not present" rather than
    // reporting a misleading 0. Once the root is open, each
    // unreadable subdirectory or vanished entry is best-effort: skip
    // it and keep accumulating, so a single permission-denied
    // subtree does not collapse the whole library size to `None`.
    let root = std::fs::read_dir(path).ok()?;
    let mut total: u64 = 0;
    let mut stack: Vec<std::path::PathBuf> = Vec::new();
    accumulate(root, &mut total, &mut stack);
    while let Some(p) = stack.pop() {
        if let Ok(read_dir) = std::fs::read_dir(&p) {
            accumulate(read_dir, &mut total, &mut stack);
        }
    }
    Some(total)
}

fn accumulate(read_dir: std::fs::ReadDir, total: &mut u64, stack: &mut Vec<std::path::PathBuf>) {
    for entry in read_dir.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                *total = total.saturating_add(meta.len());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_size_returns_none_when_root_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("never-existed");
        assert_eq!(dir_size(&missing), None);
    }

    #[test]
    fn dir_size_sums_files_across_subdirectories() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a"), b"123").expect("a");
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).expect("sub");
        std::fs::write(sub.join("b"), b"45").expect("b");
        assert_eq!(dir_size(dir.path()), Some(5));
    }

    #[cfg(unix)]
    #[test]
    fn dir_size_skips_unreadable_subdirectory_and_returns_partial_sum() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("visible"), b"1234").expect("visible");
        let locked = dir.path().join("locked");
        std::fs::create_dir_all(&locked).expect("locked");
        std::fs::write(locked.join("hidden"), b"would be 9 bytes").expect("hidden");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");
        // Root is readable; the locked subdir's entries cannot be
        // scanned. The partial sum must still come back instead of
        // collapsing to None.
        let result = dir_size(dir.path());
        // Restore permissions so the tempdir cleanup can run.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("chmod 755");
        assert_eq!(result, Some(4));
    }
}
