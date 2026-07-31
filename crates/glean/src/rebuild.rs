// SPDX-License-Identifier: Apache-2.0

//! Paper-side corpus rebuild. Peer of `bookrack_ingest::rebuild` for the
//! paper pipeline: reconstructs the corpus node tree of each rebuildable
//! paper intake from its on-disk envelope without re-running EXTRACT or
//! IDENTIFY. The abstract leaf is reseated from the `node_publication_attrs`
//! row the original glean run wrote, so the rebuilt tree carries the
//! same abstract text as before.

use std::path::Path;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_core::ItemKind;
use bookrack_corpus::{Corpus, IndexStamps};
use bookrack_extract::{EXTRACTOR_VERSION, EnvelopeError, read_envelope_with_fallback};
use bookrack_normalize::NORMALIZE_VERSION;
use bookrack_vectors::ChunkStore;

use crate::{CHUNK_VERSION, GleanError, Result, build_structure};

/// What to rebuild and how.
#[derive(Debug, Clone, Default)]
pub struct RebuildParams {
    /// When set, restrict the rebuild to this intake only. An unknown
    /// id, or one not in a rebuildable state, surfaces as
    /// [`GleanError::UnknownIntake`] / [`GleanError::IntakeNotRebuildable`].
    pub only: Option<i64>,
    /// When true, restrict the rebuild to intakes whose stored
    /// `extractor_version` does not equal [`EXTRACTOR_VERSION`].
    /// Combines with [`Self::only`] by intersection.
    pub stale_only: bool,
    /// When set, the target set is exactly this list of intake ids —
    /// [`Self::only`] and [`Self::stale_only`] are ignored. Each id
    /// must resolve to an existing catalog row in a rebuildable
    /// state; any unknown or non-rebuildable id aborts the whole
    /// call with [`GleanError::UnknownIntake`] /
    /// [`GleanError::IntakeNotRebuildable`].
    ///
    /// Used by destructive RPCs to pin the execute leg to the exact
    /// target set the operator confirmed during the dry-run leg.
    pub only_ids: Option<Vec<i64>>,
    /// When true, do not write anything: classify each intake into the
    /// outcome buckets but skip the actual structure call.
    pub dry_run: bool,
}

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
    /// catalog row's. The driver does not auto-reglean; the operator
    /// must decide.
    pub mismatched: Vec<i64>,
    /// Intakes the driver skipped because their envelope could not be
    /// parsed or the structure call failed.
    pub failed: Vec<(i64, String)>,
}

/// Rebuild the corpus tree of each rebuildable paper intake —
/// `Extracted`, `DedupHold`, or `Embedded` — from its envelope on disk.
pub fn rebuild_from_intakes(
    corpus: &mut Corpus,
    catalog: &Catalog,
    params: &RebuildParams,
) -> Result<RebuildReport> {
    let targets = if let Some(ids) = params.only_ids.as_deref() {
        collect_pinned_targets(catalog, ids)?
    } else {
        let mut t = collect_targets(catalog, params.only)?;
        if params.stale_only {
            let stale: std::collections::HashSet<i64> = catalog
                .stale_partitions(EXTRACTOR_VERSION)?
                .into_iter()
                .collect();
            t.retain(|i| stale.contains(&i.intake_id));
        }
        t
    };
    let mut report = RebuildReport::default();
    for intake in targets {
        let intake_id = intake.intake_id;
        let Some(stored_path) = intake.stored_path.as_deref() else {
            report.missing_envelope.push(intake_id);
            continue;
        };
        let envelope = match read_envelope_with_fallback(Path::new(stored_path)) {
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
        let abstract_text = match catalog.publication_attrs(intake_id, ItemKind::Paper) {
            Ok(Some(attrs)) => attrs.abstract_text,
            Ok(None) => None,
            Err(err) => {
                report.failed.push((intake_id, err.to_string()));
                continue;
            }
        };
        match build_structure(
            corpus,
            intake_id,
            abstract_text,
            &envelope.extraction.blocks,
        ) {
            Ok(_) => report.rebuilt.push(intake_id),
            Err(e) => report.failed.push((intake_id, e.to_string())),
        }
    }
    Ok(report)
}

/// Stamp `papers_corpus.db`'s `index_meta` from the dimension currently
/// on disk in `lancedb_papers`. Mirrors
/// [`bookrack_ingest::stamp_index_from_existing_chunks`] for the paper
/// store: use after a rebuild that refreshed the corpus tree without
/// touching the chunks table.
///
/// Returns `Ok(true)` if stamps were written or already matched,
/// `Ok(false)` if the chunks table is missing or empty.
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
    corpus.reconcile_index_stamps(&IndexStamps {
        embed_model: embed_model.to_string(),
        vector_dim: dim,
        chunk_version: CHUNK_VERSION,
        normalize_version: NORMALIZE_VERSION,
    })?;
    Ok(true)
}

fn collect_pinned_targets(catalog: &Catalog, ids: &[i64]) -> Result<Vec<bookrack_catalog::Intake>> {
    ids.iter()
        .map(|id| {
            let intake = catalog
                .intake_by_id(*id)?
                .ok_or(GleanError::UnknownIntake(*id))?;
            if !is_rebuildable(intake.status) {
                return Err(GleanError::IntakeNotRebuildable(*id));
            }
            Ok(intake)
        })
        .collect()
}

fn collect_targets(catalog: &Catalog, only: Option<i64>) -> Result<Vec<bookrack_catalog::Intake>> {
    Ok(match only {
        Some(id) => {
            let intake = catalog
                .intake_by_id(id)?
                .ok_or(GleanError::UnknownIntake(id))?;
            if !is_rebuildable(intake.status) {
                return Err(GleanError::IntakeNotRebuildable(id));
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
                out.extend(catalog.intakes_with_status(status)?);
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
    use bookrack_core::PartitionIdx;
    use bookrack_extract::envelope::{envelope_filename, write_envelope};
    use bookrack_extract::{
        Biblio, Block, BlockKind, Contributor, Extraction, Provenance, SkippedUnit,
        TextLayerQuality, Toc,
    };
    use tempfile::TempDir;

    fn sample_extraction() -> Extraction {
        Extraction {
            biblio: Biblio {
                title: Some("Synthetic Findings in Test Spaces".to_string()),
                subtitle: None,
                publisher: None,
                year: Some(2020),
                year_raw: Some("2020".to_string()),
                isbn: None,
                series: None,
                language: Some("en".to_string()),
                contributors: Vec::<Contributor>::new(),
                doi: None,
                arxiv_id: None,
                issn: None,
                container_title: None,
                abstract_text: None,
                csl_type: None,
            },
            blocks: vec![Block {
                kind: BlockKind::Body,
                text: "Body paragraph of the synthetic sample paper.".to_string(),
                source_unit: 0,
                style: None,
            }],
            toc: Toc::default(),
            provenance: Provenance {
                adapter: "pdf".to_string(),
                extractor_version: 1,
                text_layer_quality: TextLayerQuality::Usable,
                skipped_units: Vec::<SkippedUnit>::new(),
                derived_from_sha256: None,
                partial_pages: None,
                source_of_structure: None,
                fallbacks: Vec::new(),
            },
        }
    }

    /// Register a paper intake at `Extracted` with an envelope on disk
    /// whose `source_sha256` matches the catalog row.
    fn seed_extracted(catalog: &mut Catalog, dir: &Path, sha: &str) -> i64 {
        let intake = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new(sha.to_string()).format("pdf".to_string()),
            )
            .expect("register intake");
        let intake_id = intake.intake().intake_id;
        let envelope_path = dir.join(envelope_filename(ItemKind::Paper, intake_id));
        write_envelope(&envelope_path, &sample_extraction(), intake_id, sha)
            .expect("write envelope");
        catalog
            .set_stored_path(ItemKind::Paper, intake_id, &envelope_path.to_string_lossy())
            .expect("set stored path");
        catalog
            .set_intake_status(ItemKind::Paper, intake_id, IntakeStatus::Extracted)
            .expect("set status");
        intake_id
    }

    fn partition_leaves(corpus: &Corpus, intake_id: i64) -> Vec<bookrack_corpus::Node> {
        corpus
            .leaves_in_doc_span(PartitionIdx::new(intake_id).root(), 0, i64::MAX, 100)
            .expect("leaves query")
    }

    #[test]
    fn an_unknown_id_in_only_ids_aborts_the_whole_call() {
        let dir = TempDir::new().unwrap();
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let known = seed_extracted(&mut catalog, dir.path(), "feedface01");

        let params = RebuildParams {
            only_ids: Some(vec![known, 9_999]),
            ..RebuildParams::default()
        };
        let err = rebuild_from_intakes(&mut corpus, &catalog, &params)
            .expect_err("an unknown pinned id must abort");
        assert!(
            matches!(err, GleanError::UnknownIntake(9_999)),
            "got {err:?}"
        );
        // The whole call aborted: no partial work for the known intake.
        assert!(partition_leaves(&corpus, known).is_empty());
    }

    #[test]
    fn a_non_rebuildable_id_in_only_ids_aborts_the_whole_call() {
        let dir = TempDir::new().unwrap();
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let known = seed_extracted(&mut catalog, dir.path(), "feedface01");
        // A second intake left at the initial (non-rebuildable) status.
        let pending = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new("feedface02".to_string()).format("pdf".to_string()),
            )
            .expect("register intake")
            .intake()
            .intake_id;

        let params = RebuildParams {
            only_ids: Some(vec![known, pending]),
            ..RebuildParams::default()
        };
        let err = rebuild_from_intakes(&mut corpus, &catalog, &params)
            .expect_err("a non-rebuildable pinned id must abort");
        match err {
            GleanError::IntakeNotRebuildable(id) => assert_eq!(id, pending),
            other => panic!("expected IntakeNotRebuildable, got {other:?}"),
        }
        assert!(partition_leaves(&corpus, known).is_empty());
    }

    #[test]
    fn only_ids_pins_the_target_set_and_ignores_only_and_stale_only() {
        let dir = TempDir::new().unwrap();
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let pinned = seed_extracted(&mut catalog, dir.path(), "feedface01");
        let unpinned = seed_extracted(&mut catalog, dir.path(), "feedface02");

        // `only` names an unknown id and `stale_only` is on: were either
        // consulted, the call would abort with UnknownIntake or filter
        // the target away. The pinned list alone decides.
        let params = RebuildParams {
            only: Some(9_999),
            stale_only: true,
            only_ids: Some(vec![pinned]),
            dry_run: false,
        };
        let report = rebuild_from_intakes(&mut corpus, &catalog, &params).expect("rebuild");
        assert_eq!(report.rebuilt, vec![pinned]);
        assert!(!partition_leaves(&corpus, pinned).is_empty());
        assert!(partition_leaves(&corpus, unpinned).is_empty());
    }

    #[test]
    fn dry_run_classifies_but_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let intake_id = seed_extracted(&mut catalog, dir.path(), "feedface01");

        let dry = RebuildParams {
            dry_run: true,
            ..RebuildParams::default()
        };
        let report = rebuild_from_intakes(&mut corpus, &catalog, &dry).expect("dry run");
        assert_eq!(report.rebuilt, vec![intake_id]);
        assert!(
            partition_leaves(&corpus, intake_id).is_empty(),
            "a dry run must not write the corpus tree"
        );

        // The same params minus dry_run do write — proving the
        // emptiness above measured the skip, not a broken fixture.
        let wet = RebuildParams::default();
        let report = rebuild_from_intakes(&mut corpus, &catalog, &wet).expect("rebuild");
        assert_eq!(report.rebuilt, vec![intake_id]);
        assert!(!partition_leaves(&corpus, intake_id).is_empty());
    }

    #[test]
    fn missing_and_mismatched_envelopes_land_in_their_buckets() {
        let dir = TempDir::new().unwrap();
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        // No stored_path at all.
        let missing = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new("feedface01".to_string()).format("pdf".to_string()),
            )
            .expect("register intake")
            .intake()
            .intake_id;
        catalog
            .set_intake_status(ItemKind::Paper, missing, IntakeStatus::Extracted)
            .expect("set status");

        // An envelope whose recorded sha differs from the catalog row.
        let mismatched = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new("feedface02".to_string()).format("pdf".to_string()),
            )
            .expect("register intake")
            .intake()
            .intake_id;
        let envelope_path = dir
            .path()
            .join(envelope_filename(ItemKind::Paper, mismatched));
        write_envelope(
            &envelope_path,
            &sample_extraction(),
            mismatched,
            "0ddba11c0ffee",
        )
        .expect("write envelope");
        catalog
            .set_stored_path(
                ItemKind::Paper,
                mismatched,
                &envelope_path.to_string_lossy(),
            )
            .expect("set stored path");
        catalog
            .set_intake_status(ItemKind::Paper, mismatched, IntakeStatus::Extracted)
            .expect("set status");

        let report =
            rebuild_from_intakes(&mut corpus, &catalog, &RebuildParams::default()).expect("run");
        assert_eq!(report.missing_envelope, vec![missing]);
        assert_eq!(report.mismatched, vec![mismatched]);
        assert!(report.rebuilt.is_empty());
        assert!(report.failed.is_empty());
    }
}
