// SPDX-License-Identifier: Apache-2.0

//! On-disk metadata describing the ANN index attached to the chunks
//! table.
//!
//! The file lives at `<lancedb_dir>/vectors_meta.json` and records the
//! ANN configuration the table was last built with: the algorithm kind,
//! its IVF parameters, the chunk count at build time, and the churn
//! accumulated since. Search reads it to recover default `nprobes` and
//! `refine_factor`; the embed run reads it to decide whether a churn
//! threshold has tripped and a retrain is due.
//!
//! Writing goes through [`store`], which lays the new JSON down through
//! a sibling tempfile and renames it into place — a reader catches
//! either the complete old or the complete new file, never a torn
//! partial write.
//!
//! Schema evolution is via optional fields and [`SCHEMA_VERSION`]: new
//! fields default in [`load`], and the version is read so callers can
//! warn on a future bump.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{Result, VectorsError};

/// Current schema version of [`VectorsMeta`]. Bumped any time a field
/// is removed or its semantics change; new optional fields can be added
/// without a bump.
pub const SCHEMA_VERSION: u32 = 1;

/// Filename under `<lancedb_dir>` that carries [`VectorsMeta`].
pub const META_FILENAME: &str = "vectors_meta.json";

/// LanceDB index name the build / drop paths use. Held in the meta as
/// `lance_index_name` so a future schema can rename it without touching
/// the code that looks it up.
pub const DEFAULT_INDEX_NAME: &str = "vector_idx";

/// The persisted ANN configuration plus the bookkeeping the embed run
/// uses to decide when to retrain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorsMeta {
    /// Schema version of this file. Readers may warn on mismatch but
    /// should still try to deserialize — fields are added as optional.
    pub schema_version: u32,
    /// Lowest [`bookrack_dbkit::READER_VERSION`] a reader must be at to
    /// safely interpret this file and the chunk rows it points to.
    /// `None` on files written before the field landed; new writes
    /// stamp it with [`crate::MIN_READER_VERSION`].
    #[serde(default)]
    pub min_reader_version: Option<u32>,
    /// Kebab-case label of the index family —
    /// `"ivf-flat" / "ivf-sq" / "ivf-pq" / "ivf-hnsw-flat" /
    /// "ivf-hnsw-sq" / "ivf-hnsw-pq" / "brute-force"`.
    pub kind: String,
    /// IVF partition count (`k` for the k-means quantizer).
    pub num_partitions: u32,
    /// PQ sub-vector count; only populated for `ivf-pq*` kinds.
    #[serde(default)]
    pub num_sub_vectors: Option<u32>,
    /// PQ code width in bits per sub-vector; only populated for
    /// `ivf-pq*` kinds.
    #[serde(default)]
    pub num_bits: Option<u32>,
    /// Default `nprobes` for queries; overridable per request.
    pub default_nprobes: u32,
    /// Default `refine_factor` for queries; overridable per request.
    #[serde(default)]
    pub default_refine_factor: Option<u32>,
    /// RFC 3339 timestamp the index was last built at. Stamped by the
    /// caller; this module does not touch the clock.
    pub built_at: String,
    /// Total chunk rows at the time the index was last built.
    pub built_at_chunk_count: u64,
    /// Accumulated `abs(insert) + abs(delete)` since the index was last
    /// built. Compared against a threshold to decide retrain.
    pub churn_since_rebuild: u64,
    /// LanceDB index name (the handle `Table::list_indices` returns).
    pub lance_index_name: String,
}

fn meta_path(lancedb_dir: &Path) -> PathBuf {
    lancedb_dir.join(META_FILENAME)
}

/// Read `<lancedb_dir>/vectors_meta.json`. Returns `Ok(None)` if the
/// file does not exist (a fresh or pre-ANN library), an error on any
/// other IO or parse failure.
pub fn load(lancedb_dir: &Path) -> Result<Option<VectorsMeta>> {
    let path = meta_path(lancedb_dir);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(VectorsError::MetaIo(e)),
    };
    let meta = serde_json::from_slice(&bytes)?;
    Ok(Some(meta))
}

/// Write `<lancedb_dir>/vectors_meta.json` atomically. The new JSON
/// lands on a sibling temp file first and is renamed into place — a
/// concurrent reader sees the old file or the new file, never a torn
/// partial.
pub fn store(lancedb_dir: &Path, meta: &VectorsMeta) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(meta)?;
    let mut tmp = NamedTempFile::new_in(lancedb_dir)?;
    tmp.write_all(&bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(meta_path(lancedb_dir))
        .map_err(|e| VectorsError::MetaIo(e.error))?;
    Ok(())
}

/// Remove `<lancedb_dir>/vectors_meta.json`. Idempotent — a missing
/// file is not an error.
pub fn remove(lancedb_dir: &Path) -> Result<()> {
    let path = meta_path(lancedb_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(VectorsError::MetaIo(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_meta() -> VectorsMeta {
        VectorsMeta {
            schema_version: SCHEMA_VERSION,
            min_reader_version: Some(crate::MIN_READER_VERSION),
            kind: "ivf-flat".to_string(),
            num_partitions: 64,
            num_sub_vectors: None,
            num_bits: None,
            default_nprobes: 40,
            default_refine_factor: None,
            built_at: "2026-06-03T17:47:00Z".to_string(),
            built_at_chunk_count: 66_703,
            churn_since_rebuild: 0,
            lance_index_name: DEFAULT_INDEX_NAME.to_string(),
        }
    }

    #[test]
    fn roundtrip_preserves_all_fields() {
        let dir = TempDir::new().expect("temp dir");
        let original = sample_meta();
        store(dir.path(), &original).expect("store");
        let read = load(dir.path()).expect("load").expect("present");
        assert_eq!(read, original);
    }

    #[test]
    fn roundtrip_preserves_pq_fields_when_present() {
        let dir = TempDir::new().expect("temp dir");
        let original = VectorsMeta {
            kind: "ivf-pq".to_string(),
            num_sub_vectors: Some(128),
            num_bits: Some(8),
            default_refine_factor: Some(10),
            ..sample_meta()
        };
        store(dir.path(), &original).expect("store");
        let read = load(dir.path()).expect("load").expect("present");
        assert_eq!(read, original);
    }

    #[test]
    fn load_returns_none_when_file_is_absent() {
        let dir = TempDir::new().expect("temp dir");
        assert!(load(dir.path()).expect("load").is_none());
    }

    #[test]
    fn load_returns_a_parse_error_on_invalid_json() {
        let dir = TempDir::new().expect("temp dir");
        std::fs::write(dir.path().join(META_FILENAME), b"not json").expect("seed corrupt file");
        let err = load(dir.path()).unwrap_err();
        assert!(matches!(err, VectorsError::MetaParse(_)), "got {err:?}");
    }

    #[test]
    fn store_overwrites_an_existing_file() {
        let dir = TempDir::new().expect("temp dir");
        store(dir.path(), &sample_meta()).expect("first store");
        let updated = VectorsMeta {
            churn_since_rebuild: 12_345,
            ..sample_meta()
        };
        store(dir.path(), &updated).expect("second store");
        let read = load(dir.path()).expect("load").expect("present");
        assert_eq!(read.churn_since_rebuild, 12_345);
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = TempDir::new().expect("temp dir");
        // Removing a missing file is fine.
        remove(dir.path()).expect("first remove");
        store(dir.path(), &sample_meta()).expect("store");
        remove(dir.path()).expect("second remove");
        assert!(load(dir.path()).expect("load").is_none());
        // Calling again after the file is gone stays fine.
        remove(dir.path()).expect("third remove");
    }

    #[test]
    fn store_writes_pretty_json_with_a_trailing_newline_or_not() {
        // Documents what's on disk so a future reader is not surprised
        // by the formatting choice.
        let dir = TempDir::new().expect("temp dir");
        store(dir.path(), &sample_meta()).expect("store");
        let bytes = std::fs::read(dir.path().join(META_FILENAME)).expect("read raw");
        let s = std::str::from_utf8(&bytes).expect("utf8");
        assert!(s.contains("\"kind\": \"ivf-flat\""), "got {s}");
        assert!(s.contains("\"num_partitions\": 64"), "got {s}");
    }
}
