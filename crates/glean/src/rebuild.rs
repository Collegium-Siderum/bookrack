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
