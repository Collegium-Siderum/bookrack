// SPDX-License-Identifier: Apache-2.0

//! L0 rebuild: regenerate corpus tree (`nodes` table) from the opaque
//! store envelopes, without re-extracting any source file.
//!
//! For each intake whose lifecycle is past `Extracted` and whose
//! `stored_path` points to a readable v1 envelope, [`rebuild_from_intakes`]
//! reads back the cached [`Extraction`] and runs [`ingest_structure`] on
//! it. The chunks live in LanceDB and are not touched here — search
//! continues to use the existing vectors, which still reference valid
//! node ids because partition layout is deterministic from `intake_id`
//! and the rebuild reproduces the same nodes.
//!
//! Pair with [`reembed_all`] for a `--include-vectors` flow: nodes are
//! rebuilt here, vectors are refreshed there.
//!
//! [`reembed_all`]: crate::reembed::reembed_all

use std::path::Path;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_core::NodeType;
use bookrack_corpus::Corpus;
use bookrack_extract::EXTRACTOR_VERSION;
use bookrack_vectors::ChunkStore;

use crate::{IngestError, Result, StructureParams, current_index_stamps, ingest_structure};
use bookrack_extract::envelope::{self, EnvelopeError};

/// Per-intake outcome bucket the driver fills in.
#[derive(Debug, Clone, Default)]
pub struct RebuildReport {
    /// Intakes whose corpus tree was successfully rebuilt from the
    /// envelope.
    pub rebuilt: Vec<i64>,
    /// Intakes whose `intake.stored_path` is empty or whose envelope
    /// file does not exist on disk.
    pub missing_envelope: Vec<i64>,
    /// Intakes whose envelope's `source_sha256` did not match the
    /// catalog row's. The driver does not auto-reingest; the user
    /// must decide.
    pub mismatched: Vec<i64>,
    /// Intakes the driver skipped because their envelope could not be
    /// parsed (corrupt JSON, schema_version drift, etc).
    pub failed: Vec<(i64, String)>,
}

/// What to rebuild and how.
#[derive(Debug, Clone, Default)]
pub struct RebuildParams {
    /// STRUCTURE tuning — defaults match `IngestParams::default()`.
    pub structure: StructureParams,
    /// When set, restrict the rebuild to this intake only. Unknown
    /// id or one not in a rebuildable state returns
    /// [`IngestError::UnknownIntake`] / [`IngestError::IntakeNotEmbedded`]
    /// — the latter reuses the "not in a rebuildable state" semantics.
    pub only: Option<i64>,
    /// When true, restrict the rebuild to intakes whose stored
    /// `extractor_version` does not equal this binary's
    /// [`EXTRACTOR_VERSION`] — the partitions whose derived content
    /// was produced by an older extractor and so most needs a refresh.
    /// Combines with [`Self::only`] by intersection.
    pub stale_only: bool,
    /// When true, do not write anything: produce a [`RebuildReport`]
    /// that classifies each intake (rebuildable / missing_envelope /
    /// mismatched / failed) but skips the actual structure call.
    pub dry_run: bool,
}

/// Rebuild the corpus tree of each rebuildable intake — `Extracted`,
/// `DedupHold`, or `Embedded` — from its envelope on disk. Returns the
/// outcome bucket: per-intake success, per-intake skip reasons.
pub fn rebuild_from_intakes(
    corpus: &mut Corpus,
    catalog: &Catalog,
    params: &RebuildParams,
) -> Result<RebuildReport> {
    let mut targets = collect_targets(catalog, params.only)?;
    if params.stale_only {
        let stale: std::collections::HashSet<i64> = catalog
            .stale_partitions(EXTRACTOR_VERSION)
            .map_err(IngestError::from)?
            .into_iter()
            .collect();
        targets.retain(|i| stale.contains(&i.intake_id));
    }
    let mut report = RebuildReport::default();
    for intake in targets {
        let intake_id = intake.intake_id;
        let Some(stored_path) = intake.stored_path.as_deref() else {
            report.missing_envelope.push(intake_id);
            continue;
        };
        let envelope = match envelope::read_envelope_with_fallback(Path::new(stored_path)) {
            Ok(env) => env,
            Err(EnvelopeError::Io(_)) => {
                report.missing_envelope.push(intake_id);
                continue;
            }
            Err(err) => {
                report.failed.push((intake_id, err.to_string()));
                continue;
            }
        };
        if envelope.source_sha256 != intake.source_sha256 {
            report.mismatched.push(intake_id);
            continue;
        }
        if params.dry_run {
            report.rebuilt.push(intake_id);
            continue;
        }
        match ingest_structure(
            corpus,
            intake_id,
            NodeType::Work,
            &envelope.extraction,
            &params.structure,
        ) {
            Ok(_) => report.rebuilt.push(intake_id),
            Err(e) => report.failed.push((intake_id, e.to_string())),
        }
    }
    Ok(report)
}

/// Stamp `corpus.db`'s `index_meta` with the build parameters of the
/// vectors currently on disk.
///
/// Use after an L0 rebuild that refreshed the corpus tree without
/// touching the chunks table: the rebuilt `corpus.db` would otherwise
/// carry no stamps, and the query path's `verify_index_stamps` gate
/// would refuse to serve until a separate `vectors reembed` writes
/// them. Reads the vector dimension from the chunks table's schema —
/// the on-disk source of truth — and takes the embedding model from
/// `embed_model` (the caller's configured runtime value). The chunk
/// and normalize versions come from this binary's compiled-in
/// constants on the documented assumption that a rebuild does not
/// bump them; if it did, the vectors are stale and a reembed is the
/// right path, not this helper.
///
/// Returns `Ok(true)` if stamps were written or already matched,
/// `Ok(false)` if the chunks table is missing or empty (nothing to
/// stamp against — the caller is expected to embed before serving).
/// A stamp mismatch — pre-existing stamps that disagree with the
/// inferred ones — is propagated as [`IngestError`].
pub async fn stamp_index_from_existing_chunks(
    corpus: &Corpus,
    lancedb_dir: &Path,
    embed_model: &str,
) -> Result<bool> {
    let Some(store) = ChunkStore::try_open(lancedb_dir).await? else {
        return Ok(false);
    };
    if store.count_rows().await? == 0 {
        return Ok(false);
    }
    let dim = store.dimension() as u32;
    corpus.reconcile_index_stamps(&current_index_stamps(embed_model, dim))?;
    Ok(true)
}

fn collect_targets(catalog: &Catalog, only: Option<i64>) -> Result<Vec<bookrack_catalog::Intake>> {
    Ok(match only {
        Some(id) => {
            let intake = catalog
                .intake_by_id(id)
                .map_err(IngestError::from)?
                .ok_or(IngestError::UnknownIntake(id))?;
            if !is_rebuildable(intake.status) {
                return Err(IngestError::IntakeNotEmbedded(id));
            }
            vec![intake]
        }
        None => {
            let mut out = Vec::new();
            for status in [
                IntakeStatus::Extracted,
                IntakeStatus::DedupHold,
                IntakeStatus::Embedded,
            ] {
                out.extend(
                    catalog
                        .intakes_with_status(status)
                        .map_err(IngestError::from)?,
                );
            }
            out.sort_by_key(|i| i.intake_id);
            out
        }
    })
}

fn is_rebuildable(status: IntakeStatus) -> bool {
    matches!(
        status,
        IntakeStatus::Extracted | IntakeStatus::DedupHold | IntakeStatus::Embedded
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::NewIntake;
    use bookrack_core::ItemKind;
    use bookrack_extract::{
        Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc,
    };
    use tempfile::tempdir;

    use bookrack_extract::envelope::{envelope_filename, write_envelope};

    fn sample_extraction() -> Extraction {
        Extraction {
            blocks: vec![
                Block {
                    kind: BlockKind::Heading { level: 1 },
                    text: "Chapter One".into(),
                    source_unit: 0,
                },
                Block {
                    kind: BlockKind::Body,
                    text: "Some sample prose for rebuild.".into(),
                    source_unit: 0,
                },
            ],
            toc: Toc::default(),
            biblio: Biblio::default(),
            provenance: Provenance {
                adapter: "txt".into(),
                extractor_version: 1,
                text_layer_quality: TextLayerQuality::BornDigital,
                skipped_units: vec![],
                derived_from_sha256: None,
                partial_pages: None,
            },
        }
    }

    fn register(catalog: &mut Catalog, sha: &str) -> i64 {
        catalog
            .register_intake(
                ItemKind::Book,
                &NewIntake::new(sha.to_string()).format("txt").byte_size(1),
            )
            .expect("register")
            .intake()
            .intake_id
    }

    fn seed_envelope(
        books_dir: &Path,
        intake_id: i64,
        sha: &str,
        extraction: &Extraction,
    ) -> String {
        let path = books_dir.join(envelope_filename(ItemKind::Book, intake_id));
        write_envelope(&path, extraction, intake_id, sha).expect("write envelope");
        path.to_string_lossy().into_owned()
    }

    /// Seed a single chunk row into the lancedb dir so the helper
    /// has a non-empty chunks table to read its dim from. Uses a
    /// minimal 4-dim vector to keep the test cheap.
    async fn seed_one_chunk(lancedb_dir: &Path) {
        use bookrack_core::PartitionIdx;
        use bookrack_vectors::ChunkRow;
        let store = ChunkStore::open(lancedb_dir, 4).await.expect("open");
        let node = PartitionIdx::new(1).node_id(1).expect("offset in range");
        store
            .append(&[ChunkRow {
                vector: vec![0.1, 0.2, 0.3, 0.4],
                text: "seed".into(),
                start_node_id: node,
                start_char_offset: 0,
                end_node_id: node,
                end_char_offset: 4,
                norm_chunk_sha256: "sha-seed".into(),
            }])
            .await
            .expect("append");
    }

    #[tokio::test]
    async fn stamp_index_writes_stamps_when_chunks_are_present() {
        let dir = tempdir().expect("tempdir");
        let corpus = Corpus::open_in_memory().expect("corpus");
        seed_one_chunk(dir.path()).await;

        let wrote = stamp_index_from_existing_chunks(&corpus, dir.path(), "fake-model")
            .await
            .expect("stamp");
        assert!(wrote);
        // Read back the four stamps and confirm they match the
        // inferred build parameters.
        let model = corpus
            .meta_get(bookrack_corpus::EMBED_MODEL_KEY)
            .expect("meta")
            .expect("model present");
        assert_eq!(model, "fake-model");
        let dim = corpus
            .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
            .expect("meta")
            .expect("dim present");
        assert_eq!(dim, "4");
    }

    #[tokio::test]
    async fn stamp_index_reports_no_write_when_chunks_are_empty() {
        let dir = tempdir().expect("tempdir");
        let corpus = Corpus::open_in_memory().expect("corpus");
        // Create an empty chunks table by opening with a real dim;
        // count_rows is zero, and the helper should not stamp.
        let _ = ChunkStore::open(dir.path(), 4).await.expect("open");

        let wrote = stamp_index_from_existing_chunks(&corpus, dir.path(), "fake-model")
            .await
            .expect("stamp");
        assert!(!wrote);
        assert!(
            corpus
                .meta_get(bookrack_corpus::EMBED_MODEL_KEY)
                .expect("meta")
                .is_none()
        );
    }

    #[tokio::test]
    async fn stamp_index_reports_no_write_when_table_is_absent() {
        let dir = tempdir().expect("tempdir");
        let corpus = Corpus::open_in_memory().expect("corpus");
        // No ChunkStore::open call: the chunks table does not exist.
        // The helper must report `false` without creating one.

        let wrote = stamp_index_from_existing_chunks(&corpus, dir.path(), "fake-model")
            .await
            .expect("stamp");
        assert!(!wrote);
    }

    #[test]
    fn rebuild_populates_corpus_from_envelope() {
        let dir = tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = sample_extraction();

        let intake_id = register(&mut catalog, "sha-1");
        catalog
            .set_intake_status(ItemKind::Book, intake_id, IntakeStatus::Embedded)
            .expect("status");
        let path = seed_envelope(dir.path(), intake_id, "sha-1", &extraction);
        catalog
            .set_stored_path(ItemKind::Book, intake_id, &path)
            .expect("stored");

        let report = rebuild_from_intakes(&mut corpus, &catalog, &RebuildParams::default())
            .expect("rebuild");
        assert_eq!(report.rebuilt, vec![intake_id]);
        assert!(report.missing_envelope.is_empty());
        assert!(report.mismatched.is_empty());
        assert!(report.failed.is_empty());

        let partition = corpus
            .partition_for_intake(intake_id)
            .expect("lookup")
            .expect("present");
        assert!(
            corpus
                .book_nodes(partition.book_root_id)
                .expect("nodes")
                .len()
                > 1
        );
    }

    #[test]
    fn stale_only_skips_partitions_at_current_extractor_version() {
        let dir = tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = sample_extraction();

        // One row that has been bumped to the current extractor version
        // (so it is NOT stale) and one that is still at the default
        // (also not stale today; pretend a future binary is at v2).
        let fresh = register(&mut catalog, "sha-fresh");
        catalog
            .set_intake_status(ItemKind::Book, fresh, IntakeStatus::Embedded)
            .expect("status");
        catalog
            .set_extraction(
                ItemKind::Book,
                fresh,
                "txt",
                bookrack_extract::EXTRACTOR_VERSION,
            )
            .expect("stamp fresh");
        let path = seed_envelope(dir.path(), fresh, "sha-fresh", &extraction);
        catalog
            .set_stored_path(ItemKind::Book, fresh, &path)
            .expect("stored");

        let stale = register(&mut catalog, "sha-stale");
        catalog
            .set_intake_status(ItemKind::Book, stale, IntakeStatus::Embedded)
            .expect("status");
        catalog
            .set_extraction(
                ItemKind::Book,
                stale,
                "txt",
                bookrack_extract::EXTRACTOR_VERSION + 99,
            )
            .expect("stamp stale");
        let path = seed_envelope(dir.path(), stale, "sha-stale", &extraction);
        catalog
            .set_stored_path(ItemKind::Book, stale, &path)
            .expect("stored");

        // Without the filter, both rebuild; with it, only the stale one.
        let report = rebuild_from_intakes(
            &mut corpus,
            &catalog,
            &RebuildParams {
                stale_only: true,
                dry_run: true,
                ..Default::default()
            },
        )
        .expect("rebuild");
        assert_eq!(report.rebuilt, vec![stale]);
    }

    #[test]
    fn missing_stored_path_lands_in_missing_envelope() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let intake_id = register(&mut catalog, "sha-1");
        catalog
            .set_intake_status(ItemKind::Book, intake_id, IntakeStatus::Embedded)
            .expect("status");

        let report = rebuild_from_intakes(&mut corpus, &catalog, &RebuildParams::default())
            .expect("rebuild");
        assert_eq!(report.missing_envelope, vec![intake_id]);
        assert!(report.rebuilt.is_empty());
    }

    #[test]
    fn sha_mismatch_lands_in_mismatched_and_skips_rebuild() {
        let dir = tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = sample_extraction();

        let intake_id = register(&mut catalog, "sha-real");
        catalog
            .set_intake_status(ItemKind::Book, intake_id, IntakeStatus::Embedded)
            .expect("status");
        // Envelope records a different sha than the intake row.
        let path = seed_envelope(dir.path(), intake_id, "sha-other", &extraction);
        catalog
            .set_stored_path(ItemKind::Book, intake_id, &path)
            .expect("stored");

        let report = rebuild_from_intakes(&mut corpus, &catalog, &RebuildParams::default())
            .expect("rebuild");
        assert_eq!(report.mismatched, vec![intake_id]);
        assert!(report.rebuilt.is_empty());
        assert!(
            corpus
                .partition_for_intake(intake_id)
                .expect("lookup")
                .is_none(),
            "mismatched envelope must not write any corpus nodes"
        );
    }

    #[test]
    fn dry_run_classifies_without_writing() {
        let dir = tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = sample_extraction();

        let with_envelope = register(&mut catalog, "sha-a");
        catalog
            .set_intake_status(ItemKind::Book, with_envelope, IntakeStatus::Embedded)
            .expect("status");
        let path = seed_envelope(dir.path(), with_envelope, "sha-a", &extraction);
        catalog
            .set_stored_path(ItemKind::Book, with_envelope, &path)
            .expect("stored");

        let without_envelope = register(&mut catalog, "sha-b");
        catalog
            .set_intake_status(ItemKind::Book, without_envelope, IntakeStatus::Embedded)
            .expect("status");

        let params = RebuildParams {
            dry_run: true,
            ..RebuildParams::default()
        };
        let report = rebuild_from_intakes(&mut corpus, &catalog, &params).expect("rebuild");
        assert_eq!(report.rebuilt, vec![with_envelope]);
        assert_eq!(report.missing_envelope, vec![without_envelope]);
        assert!(
            corpus
                .partition_for_intake(with_envelope)
                .expect("lookup")
                .is_none(),
            "dry_run must not write any corpus nodes"
        );
    }
}
