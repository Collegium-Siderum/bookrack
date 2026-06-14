// SPDX-License-Identifier: Apache-2.0

//! Paper-side reset+rechunk. Peer of `bookrack_ingest::reset` for the
//! paper pipeline: drops the chunks table, demotes every `Embedded`
//! paper intake to `Extracted`, then re-chunks the abstract leaf of
//! each one under the current [`CHUNK_VERSION`] and re-embeds it under
//! the active embedder.

use std::path::Path;
use std::time::Instant;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_config::EmbedConfig;
use bookrack_core::{ItemKind, NodeType, PartitionIdx};
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_vectors::ChunkStore;

use crate::{ChunkParams, Result, audit_as, embed_and_write_chunks, new_run_id, plan_chunks};

/// What one [`reset_and_rechunk`] call produced.
#[derive(Debug, Clone, Default)]
pub struct ResetReport {
    /// Intakes that were re-embedded under the new model.
    pub intakes_reembedded: usize,
    /// Total chunk rows written across all re-embedded intakes.
    pub chunks_written: usize,
    /// Intakes whose corpus tree carried no abstract leaf, so
    /// chunking produced no plans. Their catalog status is left at
    /// `Extracted` so a follow-up can pick them up.
    pub skipped_empty: Vec<i64>,
    /// The first intake an embed call failed on, if any.
    pub failed_intake: Option<i64>,
}

/// Drop `lancedb_papers`'s chunks table, demote every `Embedded`
/// paper intake to `Extracted`, then re-chunk and re-embed each from
/// its abstract leaf in `papers_corpus`.
///
/// On `resume = false`:
///   1. clears the corpus `index_meta` stamps,
///   2. drops the LanceDB chunks table and removes the
///      `vectors_meta.json` sidecar,
///   3. demotes every `Embedded` intake to `Extracted`,
///   4. enters the build phase.
///
/// On `resume = true` the destructive steps 1-3 are skipped. The build
/// phase picks whatever `Extracted` intakes remain, meant for the case
/// where an earlier reset was interrupted mid-build.
pub async fn reset_and_rechunk<E: Embedder>(
    catalog: &Catalog,
    corpus: &mut Corpus,
    lancedb_dir: &Path,
    embedder: &E,
    cfg: &EmbedConfig,
    resume: bool,
) -> Result<ResetReport> {
    if !resume {
        corpus.clear_index_stamps()?;
        ChunkStore::drop_chunks_table(lancedb_dir).await?;
        let embedded: Vec<i64> = catalog
            .intakes_with_status(IntakeStatus::Embedded)?
            .into_iter()
            .map(|i| i.intake_id)
            .collect();
        for id in embedded {
            catalog.set_intake_status(ItemKind::Paper, id, IntakeStatus::Extracted)?;
        }
    }

    let mut report = ResetReport::default();
    let targets = catalog.intakes_with_status(IntakeStatus::Extracted)?;
    let chunk_params = ChunkParams::default();
    let run_id = new_run_id("reset");

    for intake in targets {
        let intake_id = intake.intake_id;
        let sha = intake.source_sha256.as_str();
        let work_root = PartitionIdx::new(intake_id).root();
        let work_root_raw = work_root.get();

        let started = Instant::now();
        // The abstract leaf is the leaf at toc position 0. Other leaf
        // kinds (heading, caption) are not chunked, so an empty result
        // or a non-Paragraph hit means there was no abstract on the
        // original glean run either; matches `glean_paper`'s
        // chunks_written = 0 branch.
        let leaves = corpus.leaves_in_doc_span(work_root, 0, 0, 1)?;
        let leaf = match leaves.first() {
            Some(leaf) if matches!(leaf.node_type, NodeType::Paragraph) => leaf,
            _ => {
                report.skipped_empty.push(intake_id);
                continue;
            }
        };
        let leaf_id = leaf.node_id;
        let abstract_text = leaf.text_content.clone().unwrap_or_default();
        let plans = plan_chunks(leaf_id, &abstract_text, &chunk_params);
        audit_as(
            catalog,
            "glean-reset",
            &run_id,
            sha,
            Some(work_root_raw),
            "chunk",
            "chunk",
            "ok",
            started,
            Some(format!(r#"{{"chunks":{}}}"#, plans.len())),
            None,
        );
        if plans.is_empty() {
            report.skipped_empty.push(intake_id);
            continue;
        }

        let started = Instant::now();
        let chunks_written =
            match embed_and_write_chunks(corpus, lancedb_dir, embedder, cfg, intake_id, &plans)
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    audit_as(
                        catalog,
                        "glean-reset",
                        &run_id,
                        sha,
                        Some(work_root_raw),
                        "embed",
                        "embed",
                        "fail",
                        started,
                        None,
                        Some(&e.to_string()),
                    );
                    report.failed_intake = Some(intake_id);
                    return Err(e);
                }
            };
        audit_as(
            catalog,
            "glean-reset",
            &run_id,
            sha,
            Some(work_root_raw),
            "embed",
            "embed",
            "ok",
            started,
            Some(format!(r#"{{"chunks":{chunks_written}}}"#)),
            None,
        );
        catalog.set_intake_status(ItemKind::Paper, intake_id, IntakeStatus::Embedded)?;
        report.intakes_reembedded += 1;
        report.chunks_written += chunks_written;
    }

    Ok(report)
}
