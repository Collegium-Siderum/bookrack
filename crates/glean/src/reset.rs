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

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    use bookrack_catalog::NewIntake;
    use bookrack_corpus::{EMBED_MODEL_KEY, VECTOR_DIM_KEY};
    use bookrack_embed::{EmbedError, Result as EmbedResult};

    use crate::{GleanError, build_structure};

    /// A fake embedder returning constant vectors of `dim` length whose
    /// second slot carries `generation`, so tests can tell rewrites
    /// apart from leftovers.
    struct Fake {
        dim: usize,
        generation: u8,
    }

    impl Embedder for Fake {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let n = texts.len();
            let dim = self.dim;
            let generation = self.generation;
            async move {
                Ok((0..n)
                    .map(|_| {
                        let mut v = vec![0.25f32; dim];
                        v[1] = generation as f32;
                        v
                    })
                    .collect())
            }
        }
    }

    /// A fake embedder that always fails, forcing the abort path.
    struct Offline;

    impl Embedder for Offline {
        fn embed_batch(
            &self,
            _texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            std::future::ready(Err(EmbedError::Unreachable(
                "test embedder offline".to_string(),
            )))
        }
    }

    fn embed_cfg(model: &str) -> EmbedConfig {
        EmbedConfig {
            model: model.to_string(),
            ..EmbedConfig::default()
        }
    }

    const ABSTRACT: &str = "Synthetic abstract prose, long enough for the \
         chunk planner to emit at least one span for the embed step.";

    /// Seed one paper intake to `Embedded` through the production write
    /// path: real structure build, real chunk plan, real embed+write.
    async fn seed_embedded(
        catalog: &mut Catalog,
        corpus: &mut Corpus,
        lancedb_dir: &Path,
        sha: &str,
        dim: usize,
    ) -> i64 {
        let intake_id = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new(sha.to_string()).format("pdf".to_string()),
            )
            .expect("register")
            .intake()
            .intake_id;
        let result =
            build_structure(corpus, intake_id, Some(ABSTRACT.to_string()), &[]).expect("structure");
        let leaf_id = result.leaf_node_id.expect("abstract leaf");
        let plans = plan_chunks(leaf_id, ABSTRACT, &ChunkParams::default());
        assert!(!plans.is_empty(), "seed needs at least one chunk plan");
        let written = embed_and_write_chunks(
            corpus,
            lancedb_dir,
            &Fake { dim, generation: 1 },
            &embed_cfg("old-model"),
            intake_id,
            &plans,
        )
        .await
        .expect("embed");
        assert!(written > 0);
        catalog
            .set_intake_status(ItemKind::Paper, intake_id, IntakeStatus::Embedded)
            .expect("status");
        intake_id
    }

    async fn partition_tags(lancedb_dir: &Path, intake_id: i64) -> Vec<f32> {
        let store = ChunkStore::try_open(lancedb_dir)
            .await
            .expect("try_open")
            .expect("chunks table");
        store
            .scan_partition(PartitionIdx::new(intake_id))
            .await
            .expect("scan")
            .iter()
            .map(|row| row.vector[1])
            .collect()
    }

    fn status_of(catalog: &Catalog, intake_id: i64) -> IntakeStatus {
        catalog
            .intake_by_id(intake_id)
            .expect("by id")
            .expect("row")
            .status
    }

    #[tokio::test]
    async fn reset_swaps_the_stamps_demotes_and_rebuilds_to_embedded() {
        const OLD_DIM: usize = 4;
        const NEW_DIM: usize = 8;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let a = seed_embedded(&mut catalog, &mut corpus, dir.path(), "sha-a", OLD_DIM).await;
        let b = seed_embedded(&mut catalog, &mut corpus, dir.path(), "sha-b", OLD_DIM).await;
        assert_eq!(
            corpus.meta_get(VECTOR_DIM_KEY).expect("get"),
            Some(OLD_DIM.to_string())
        );

        let report = reset_and_rechunk(
            &catalog,
            &mut corpus,
            dir.path(),
            &Fake {
                dim: NEW_DIM,
                generation: 9,
            },
            &embed_cfg("new-model"),
            false,
        )
        .await
        .expect("reset");

        assert_eq!(report.intakes_reembedded, 2);
        assert!(report.chunks_written > 0);
        assert!(report.skipped_empty.is_empty());
        // Stamps reflect the new model and dimension.
        assert_eq!(
            corpus.meta_get(EMBED_MODEL_KEY).expect("get"),
            Some("new-model".to_string())
        );
        assert_eq!(
            corpus.meta_get(VECTOR_DIM_KEY).expect("get"),
            Some(NEW_DIM.to_string())
        );
        // The rows were rewritten under the new generation and every
        // intake landed back at Embedded.
        for id in [a, b] {
            assert!(
                partition_tags(dir.path(), id)
                    .await
                    .iter()
                    .all(|&t| t == 9.0)
            );
            assert_eq!(status_of(&catalog, id), IntakeStatus::Embedded);
        }
    }

    #[tokio::test]
    async fn reset_aborts_on_embed_failure_and_resume_finishes_the_run() {
        const DIM: usize = 4;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let a = seed_embedded(&mut catalog, &mut corpus, dir.path(), "sha-a", DIM).await;
        let b = seed_embedded(&mut catalog, &mut corpus, dir.path(), "sha-b", DIM).await;

        let err = reset_and_rechunk(
            &catalog,
            &mut corpus,
            dir.path(),
            &Offline,
            &embed_cfg("new-model"),
            false,
        )
        .await
        .expect_err("offline embedder must abort the build phase");
        assert!(matches!(err, GleanError::Embed(_)), "got {err:?}");
        // The destructive legs ran before the abort: both intakes sit
        // demoted at Extracted.
        assert_eq!(status_of(&catalog, a), IntakeStatus::Extracted);
        assert_eq!(status_of(&catalog, b), IntakeStatus::Extracted);

        // Resume finishes the interrupted run without re-dropping.
        let report = reset_and_rechunk(
            &catalog,
            &mut corpus,
            dir.path(),
            &Fake {
                dim: DIM,
                generation: 5,
            },
            &embed_cfg("new-model"),
            true,
        )
        .await
        .expect("resume");
        assert_eq!(report.intakes_reembedded, 2);
        for id in [a, b] {
            assert!(
                partition_tags(dir.path(), id)
                    .await
                    .iter()
                    .all(|&t| t == 5.0)
            );
            assert_eq!(status_of(&catalog, id), IntakeStatus::Embedded);
        }
    }

    #[tokio::test]
    async fn resume_on_a_clean_library_is_a_noop() {
        const DIM: usize = 4;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let a = seed_embedded(&mut catalog, &mut corpus, dir.path(), "sha-a", DIM).await;

        let report = reset_and_rechunk(
            &catalog,
            &mut corpus,
            dir.path(),
            &Fake {
                dim: DIM,
                generation: 5,
            },
            &embed_cfg("old-model"),
            true,
        )
        .await
        .expect("resume");

        // Nothing was Extracted, so nothing rebuilt — and the
        // destructive steps 1-3 were skipped: stamps, table, vectors,
        // and status all still carry the seeded state.
        assert_eq!(report.intakes_reembedded, 0);
        assert_eq!(report.chunks_written, 0);
        assert!(report.skipped_empty.is_empty());
        assert_eq!(
            corpus.meta_get(EMBED_MODEL_KEY).expect("get"),
            Some("old-model".to_string())
        );
        assert!(
            partition_tags(dir.path(), a)
                .await
                .iter()
                .all(|&t| t == 1.0)
        );
        assert_eq!(status_of(&catalog, a), IntakeStatus::Embedded);
    }

    #[tokio::test]
    async fn an_intake_without_an_abstract_leaf_is_skipped_and_left_extracted() {
        const DIM: usize = 4;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        // One healthy seed so the chunks table exists.
        let healthy = seed_embedded(&mut catalog, &mut corpus, dir.path(), "sha-a", DIM).await;
        // One Extracted intake with no corpus tree at all.
        let bare = catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new("sha-bare".to_string()).format("pdf".to_string()),
            )
            .expect("register")
            .intake()
            .intake_id;
        catalog
            .set_intake_status(ItemKind::Paper, bare, IntakeStatus::Extracted)
            .expect("status");

        let report = reset_and_rechunk(
            &catalog,
            &mut corpus,
            dir.path(),
            &Fake {
                dim: DIM,
                generation: 5,
            },
            &embed_cfg("old-model"),
            true,
        )
        .await
        .expect("resume");

        assert_eq!(report.skipped_empty, vec![bare]);
        assert_eq!(report.intakes_reembedded, 0);
        // The skipped intake stays Extracted for a follow-up; the
        // healthy one was untouched by the resume run.
        assert_eq!(status_of(&catalog, bare), IntakeStatus::Extracted);
        assert_eq!(status_of(&catalog, healthy), IntakeStatus::Embedded);
    }
}
