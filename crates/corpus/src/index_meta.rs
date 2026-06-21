// SPDX-License-Identifier: Apache-2.0

//! The `index_meta` table — index-level scalars.
//!
//! `index_meta` records the parameters an index was built with —
//! embedding model, vector dimension, chunk and normalization versions,
//! the schema version — so a daemon can refuse to serve an index that no
//! longer matches its compiled-in constants.

use bookrack_dbkit::{ColumnSpec, TableSpec};
use rusqlite::params;

use crate::{Corpus, CorpusError, Result};

/// The single source of truth for the `index_meta` table's schema. Its
/// DDL is rendered from this spec.
pub(crate) const SPEC: TableSpec = TableSpec {
    name: "index_meta",
    comment: Some("Index-level scalars: the parameters an index was built with."),
    columns: &[
        ColumnSpec::text("key").primary_key(),
        ColumnSpec::text("value").not_null(),
    ],
    composite_pk: None,
    uniques: &[],
    table_checks: &[],
    indexes: &[],
};

/// `index_meta` key recording the embedding model an index was built with.
pub const EMBED_MODEL_KEY: &str = "embed_model";
/// `index_meta` key recording the vector width the dense store was fixed at.
pub const VECTOR_DIM_KEY: &str = "vector_dim";
/// `index_meta` key recording the chunking-behaviour version.
pub const CHUNK_VERSION_KEY: &str = "chunk_version";
/// `index_meta` key recording the text-normalization version.
pub const NORMALIZE_VERSION_KEY: &str = "normalize_version";

/// The build parameters an index was created with.
///
/// Recorded in `index_meta` when an index is first built and checked
/// against on every later build and serve: a mismatch means the index was
/// produced by a different embedding model or a bumped algorithm version,
/// and — since the store is rebuildable — the resolution is to rebuild it.
///
/// The values come from outside this crate: the model and vector width are
/// runtime values, and the chunk and normalize versions are constants
/// owned by the crates that define those algorithms. A caller assembles an
/// `IndexStamps` from its compiled-in constants and configured model and
/// passes it to [`Corpus::reconcile_index_stamps`] or
/// [`Corpus::verify_index_stamps`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStamps {
    /// The embedding model the chunks were embedded with.
    pub embed_model: String,
    /// The width of the stored vectors.
    pub vector_dim: u32,
    /// The chunking-behaviour version the chunks were planned with.
    pub chunk_version: u32,
    /// The normalization version the content hashes were derived with.
    pub normalize_version: u32,
}

impl Corpus {
    /// Read an `index_meta` scalar, or `None` if the key is unset.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(bookrack_dbkit::meta_get(&self.conn, SPEC.name, key)?)
    }

    /// Write an `index_meta` scalar, replacing any previous value.
    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        bookrack_dbkit::meta_set(&self.conn, SPEC.name, key, value)?;
        Ok(())
    }

    /// Clear the four build-parameter stamps, returning the index to an
    /// unstamped state.
    ///
    /// Deletes `embed_model` / `vector_dim` / `chunk_version` /
    /// `normalize_version` from `index_meta` in one statement. Keys absent
    /// from the table are silently ignored; the operation is idempotent.
    ///
    /// After this runs, the next [`Self::reconcile_index_stamps`] call
    /// writes the supplied stamps verbatim instead of validating against
    /// them, so any subsequent `embed_book_chunks` can commit a fresh model
    /// / dimension pair. The serve-side [`Self::verify_index_stamps`] gate
    /// rejects the cleared index with [`CorpusError::IndexNotStamped`]
    /// until something embeds again.
    pub fn clear_index_stamps(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM index_meta WHERE key IN (?1, ?2, ?3, ?4)",
            params![
                EMBED_MODEL_KEY,
                VECTOR_DIM_KEY,
                CHUNK_VERSION_KEY,
                NORMALIZE_VERSION_KEY,
            ],
        )?;
        Ok(())
    }

    /// Stamp the build parameters on a fresh index, or verify them on an
    /// existing one.
    ///
    /// On an unstamped index, records all of `expected`. On a stamped one,
    /// checks every value and fails with [`CorpusError::IndexStampMismatch`]
    /// on the first that differs. This is the build-side gate: it runs
    /// before the first vector is written, so a book embedded with a
    /// different model or algorithm version is refused rather than mixed
    /// into an index built with another.
    pub fn reconcile_index_stamps(&self, expected: &IndexStamps) -> Result<()> {
        if !self.check_index_stamps(expected)? {
            self.write_index_stamps(expected)?;
        }
        Ok(())
    }

    /// Verify the recorded build parameters match `expected`, refusing an
    /// index that carries none.
    ///
    /// This is the serve-side gate: a daemon opening an existing index
    /// checks it against the daemon's compiled-in constants and configured
    /// model, and refuses to serve a stale one. An index with no stamps
    /// predates version stamping and is rejected with
    /// [`CorpusError::IndexNotStamped`].
    pub fn verify_index_stamps(&self, expected: &IndexStamps) -> Result<()> {
        if !self.check_index_stamps(expected)? {
            return Err(CorpusError::IndexNotStamped);
        }
        Ok(())
    }

    /// Compare the recorded stamps to `expected`. `Ok(true)` when the index
    /// is fully stamped and every value matches, `Ok(false)` when it is not
    /// stamped at all, and [`CorpusError::IndexStampMismatch`] when it is
    /// stamped but a value differs. The presence of [`EMBED_MODEL_KEY`] is
    /// the sentinel for "stamped", since the four keys are written as a set.
    fn check_index_stamps(&self, expected: &IndexStamps) -> Result<bool> {
        let Some(model) = self.meta_get(EMBED_MODEL_KEY)? else {
            return Ok(false);
        };
        expect_stamp(EMBED_MODEL_KEY, model, &expected.embed_model)?;
        expect_stamp(
            VECTOR_DIM_KEY,
            self.meta_get(VECTOR_DIM_KEY)?.unwrap_or_default(),
            &expected.vector_dim.to_string(),
        )?;
        expect_stamp(
            CHUNK_VERSION_KEY,
            self.meta_get(CHUNK_VERSION_KEY)?.unwrap_or_default(),
            &expected.chunk_version.to_string(),
        )?;
        expect_stamp(
            NORMALIZE_VERSION_KEY,
            self.meta_get(NORMALIZE_VERSION_KEY)?.unwrap_or_default(),
            &expected.normalize_version.to_string(),
        )?;
        Ok(true)
    }

    /// Write all four build stamps in one transaction, replacing any
    /// previous values. A crash mid-write leaves the prior stamps
    /// intact instead of a partial set, so the next open sees either
    /// the old complete state or the new complete state, never a
    /// mismatch with a trailing empty key.
    fn write_index_stamps(&self, stamps: &IndexStamps) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        bookrack_dbkit::meta_set(&tx, SPEC.name, EMBED_MODEL_KEY, &stamps.embed_model)?;
        bookrack_dbkit::meta_set(
            &tx,
            SPEC.name,
            VECTOR_DIM_KEY,
            &stamps.vector_dim.to_string(),
        )?;
        bookrack_dbkit::meta_set(
            &tx,
            SPEC.name,
            CHUNK_VERSION_KEY,
            &stamps.chunk_version.to_string(),
        )?;
        bookrack_dbkit::meta_set(
            &tx,
            SPEC.name,
            NORMALIZE_VERSION_KEY,
            &stamps.normalize_version.to_string(),
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Reject a single drifted stamp, naming the key that differs.
fn expect_stamp(key: &'static str, found: String, expected: &str) -> Result<()> {
    if found == expected {
        Ok(())
    } else {
        Err(CorpusError::IndexStampMismatch {
            key,
            found,
            expected: expected.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamps() -> IndexStamps {
        IndexStamps {
            embed_model: "qwen3-embedding:0.6b".to_string(),
            vector_dim: 1024,
            chunk_version: 1,
            normalize_version: 1,
        }
    }

    #[test]
    fn reconcile_stamps_a_fresh_index() {
        let corpus = Corpus::open_in_memory().expect("open");
        corpus.reconcile_index_stamps(&stamps()).expect("stamp");
        assert_eq!(
            corpus.meta_get(EMBED_MODEL_KEY).expect("get"),
            Some("qwen3-embedding:0.6b".to_string())
        );
        assert_eq!(
            corpus.meta_get(VECTOR_DIM_KEY).expect("get"),
            Some("1024".to_string())
        );
        assert_eq!(
            corpus.meta_get(CHUNK_VERSION_KEY).expect("get"),
            Some("1".to_string())
        );
        assert_eq!(
            corpus.meta_get(NORMALIZE_VERSION_KEY).expect("get"),
            Some("1".to_string())
        );
    }

    #[test]
    fn reconcile_is_idempotent_for_matching_stamps() {
        let corpus = Corpus::open_in_memory().expect("open");
        corpus.reconcile_index_stamps(&stamps()).expect("first");
        corpus.reconcile_index_stamps(&stamps()).expect("second");
        corpus.verify_index_stamps(&stamps()).expect("verify");
    }

    #[test]
    fn a_changed_model_is_rejected() {
        let corpus = Corpus::open_in_memory().expect("open");
        corpus.reconcile_index_stamps(&stamps()).expect("stamp");
        let other = IndexStamps {
            embed_model: "different-model".to_string(),
            ..stamps()
        };
        let err = corpus
            .reconcile_index_stamps(&other)
            .expect_err("must reject");
        assert!(matches!(
            err,
            CorpusError::IndexStampMismatch { key, .. } if key == EMBED_MODEL_KEY
        ));
    }

    #[test]
    fn a_changed_dimension_is_rejected() {
        let corpus = Corpus::open_in_memory().expect("open");
        corpus.reconcile_index_stamps(&stamps()).expect("stamp");
        let other = IndexStamps {
            vector_dim: 768,
            ..stamps()
        };
        let err = corpus.verify_index_stamps(&other).expect_err("must reject");
        assert!(matches!(
            err,
            CorpusError::IndexStampMismatch { key, .. } if key == VECTOR_DIM_KEY
        ));
    }

    #[test]
    fn a_changed_chunk_version_is_rejected() {
        let corpus = Corpus::open_in_memory().expect("open");
        corpus.reconcile_index_stamps(&stamps()).expect("stamp");
        let other = IndexStamps {
            chunk_version: 2,
            ..stamps()
        };
        let err = corpus.verify_index_stamps(&other).expect_err("must reject");
        assert!(matches!(
            err,
            CorpusError::IndexStampMismatch { key, .. } if key == CHUNK_VERSION_KEY
        ));
    }

    #[test]
    fn a_changed_normalize_version_is_rejected() {
        let corpus = Corpus::open_in_memory().expect("open");
        corpus.reconcile_index_stamps(&stamps()).expect("stamp");
        let other = IndexStamps {
            normalize_version: 2,
            ..stamps()
        };
        let err = corpus.verify_index_stamps(&other).expect_err("must reject");
        assert!(matches!(
            err,
            CorpusError::IndexStampMismatch { key, .. } if key == NORMALIZE_VERSION_KEY
        ));
    }

    #[test]
    fn verify_rejects_an_unstamped_index() {
        let corpus = Corpus::open_in_memory().expect("open");
        let err = corpus
            .verify_index_stamps(&stamps())
            .expect_err("must reject");
        assert!(matches!(err, CorpusError::IndexNotStamped));
    }

    #[test]
    fn clear_on_an_unstamped_index_is_a_noop() {
        let corpus = Corpus::open_in_memory().expect("open");
        corpus.clear_index_stamps().expect("clear");
        let err = corpus
            .verify_index_stamps(&stamps())
            .expect_err("still unstamped");
        assert!(matches!(err, CorpusError::IndexNotStamped));
    }

    #[test]
    fn clear_lets_a_new_model_take_the_stamps() {
        let corpus = Corpus::open_in_memory().expect("open");
        corpus.reconcile_index_stamps(&stamps()).expect("stamp");
        corpus.clear_index_stamps().expect("clear");

        let new = IndexStamps {
            embed_model: "qwen3-embedding:4b".to_string(),
            vector_dim: 2560,
            ..stamps()
        };
        corpus
            .reconcile_index_stamps(&new)
            .expect("fresh stamps accepted");
        corpus
            .verify_index_stamps(&new)
            .expect("new stamps survive verify");
    }

    #[test]
    fn clear_removes_every_stamp_key() {
        let corpus = Corpus::open_in_memory().expect("open");
        corpus.reconcile_index_stamps(&stamps()).expect("stamp");
        corpus.clear_index_stamps().expect("clear");
        for key in [
            EMBED_MODEL_KEY,
            VECTOR_DIM_KEY,
            CHUNK_VERSION_KEY,
            NORMALIZE_VERSION_KEY,
        ] {
            assert_eq!(corpus.meta_get(key).expect("get"), None, "{key} cleared");
        }
    }
}
