// SPDX-License-Identifier: Apache-2.0

//! L2 reset: drop the chunks table and re-derive vectors with the
//! current embedder model, from the corpus node tree.
//!
//! Unlike [`reembed_all`](crate::reembed::reembed_all), which reuses the
//! chunk text already in the chunks table, `reset_and_rechunk` is the
//! legitimate path through a model swap — the table is dropped before
//! being rebuilt at the new vector dimension. The catalog and the corpus
//! node tree are preserved; chunking re-runs against the existing nodes
//! via [`plan_book_chunks`](crate::plan_book_chunks).
//!
//! The destructive A-D steps (clear stamps, drop chunks table, demote
//! catalog) run once on a non-`resume` call. The build phase iterates
//! `Extracted` intakes and re-promotes them to `Embedded` per book. A
//! failure mid-build leaves successful intakes at `Embedded` and the
//! failing one at `Extracted`, so a follow-up
//! `reset_and_rechunk(..., resume = true)` continues without redoing
//! the destructive steps.

use std::path::Path;
use std::time::Instant;

use bookrack_catalog::{Catalog, IntakeStatus};
use bookrack_config::EmbedConfig;
use bookrack_core::{PartitionIdx, error_chain};
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_vectors::ChunkStore;

use crate::chunk::ChunkParams;
use crate::embed_run::embed_book_chunks;
use crate::{Result, audit_as, maintenance_run_id, plan_book_chunks};

/// What one [`reset_and_rechunk`] call produced.
#[derive(Debug, Clone, Default)]
pub struct ResetReport {
    /// Intakes that were re-embedded under the new model.
    pub intakes_reembedded: usize,
    /// Total chunk rows written across all re-embedded intakes.
    pub chunks_written: usize,
    /// Intakes whose corpus tree carried no prose leaves, so chunking
    /// produced no plans. Their catalog status is left at `Extracted`
    /// so a follow-up run can pick them up if the corpus is repaired.
    pub skipped_empty: Vec<i64>,
    /// The first intake an embed call failed on, if any. The error is
    /// returned to the caller alongside this report.
    pub failed_intake: Option<i64>,
}

/// Drop the chunks table, demote every `Embedded` intake to `Extracted`,
/// then re-chunk and re-embed each from the corpus node tree.
///
/// On `resume = false`:
///   1. clears the corpus `index_meta` stamps;
///   2. drops the LanceDB chunks table and removes the
///      `vectors_meta.json` sidecar;
///   3. demotes every `Embedded` intake to `Extracted`;
///   4. enters the build phase.
///
/// On `resume = true` the destructive steps 1-3 are skipped. The build
/// phase picks whatever `Extracted` intakes remain — meant for the
/// case where an earlier reset was interrupted mid-build.
///
/// Per-intake embed failures abort the run. Successful intakes up to
/// that point keep their `Embedded` status; the failing intake stays at
/// `Extracted`. The caller can retry by invoking this with
/// `resume = true`.
pub async fn reset_and_rechunk<E: Embedder>(
    catalog: &Catalog,
    corpus: &Corpus,
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
            catalog.set_intake_status(id, IntakeStatus::Extracted)?;
        }
    }

    let mut report = ResetReport::default();
    let targets = catalog.intakes_with_status(IntakeStatus::Extracted)?;
    let chunk_params = ChunkParams::default();
    let run_id = maintenance_run_id("reset");

    for intake in targets {
        let intake_id = intake.intake_id;
        let sha = intake.source_sha256.as_str();
        let book_root_id = PartitionIdx::new(intake_id).root();
        let book_root_raw = book_root_id.get();

        let started = Instant::now();
        let plans = match plan_book_chunks(corpus, book_root_id, &chunk_params) {
            Ok(p) => p,
            Err(e) => {
                audit_as(
                    catalog,
                    "reset",
                    &run_id,
                    sha,
                    Some(book_root_raw),
                    "chunk",
                    "chunk",
                    "fail",
                    started,
                    None,
                    Some(&error_chain(&e)),
                );
                report.failed_intake = Some(intake_id);
                return Err(e);
            }
        };
        audit_as(
            catalog,
            "reset",
            &run_id,
            sha,
            Some(book_root_raw),
            "chunk",
            "chunk",
            "ok",
            started,
            Some(format!(r#"{{"chunks":{}}}"#, plans.len())),
            None,
        );
        if plans.is_empty() {
            // No prose leaves to chunk. Leave the intake at Extracted
            // so a future repair-then-resume can pick it up, and
            // report it explicitly rather than silently swallow.
            report.skipped_empty.push(intake_id);
            continue;
        }

        let started = Instant::now();
        let embed_run = match embed_book_chunks(&plans, embedder, corpus, lancedb_dir, cfg).await {
            Ok(r) => r,
            Err(e) => {
                audit_as(
                    catalog,
                    "reset",
                    &run_id,
                    sha,
                    Some(book_root_raw),
                    "embed",
                    "embed",
                    "fail",
                    started,
                    None,
                    Some(&error_chain(&e)),
                );
                report.failed_intake = Some(intake_id);
                return Err(e);
            }
        };
        audit_as(
            catalog,
            "reset",
            &run_id,
            sha,
            Some(book_root_raw),
            "embed",
            "embed",
            "ok",
            started,
            Some(format!(
                r#"{{"chunks":{},"batches":{},"shrink_events":{},"chars":{}}}"#,
                embed_run.chunks_written,
                embed_run.batches,
                embed_run.shrink_events,
                embed_run.total_chars
            )),
            None,
        );
        catalog.set_intake_status(intake_id, IntakeStatus::Embedded)?;
        report.intakes_reembedded += 1;
        report.chunks_written += embed_run.chunks_written;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::io::Write;

    use bookrack_catalog::Catalog;
    use bookrack_corpus::{Corpus, EMBED_MODEL_KEY, VECTOR_DIM_KEY};
    use bookrack_embed::{EmbedError, Embedder, Result as EmbedResult};

    use crate::{IngestError, IngestParams, ingest_book};

    /// A fake embedder returning constant-length vectors. The first byte
    /// of every output vector carries `tag`, which tests use to tell
    /// generations apart.
    struct Fake {
        dim: usize,
        tag: f32,
    }

    impl Embedder for Fake {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let dim = self.dim;
            let tag = self.tag;
            let n = texts.len();
            async move {
                Ok((0..n)
                    .map(|_| {
                        let mut v = vec![0.25f32; dim];
                        v[0] = tag;
                        v
                    })
                    .collect())
            }
        }
    }

    /// A fake embedder that always fails on `embed_batch`. Used to force
    /// the build-phase failure path.
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

    /// Write a tiny plain-text book under `dir`; each non-blank line
    /// becomes a block when extracted.
    fn write_sample(dir: &Path, basename: &str) -> std::path::PathBuf {
        let path = dir.join(format!("{basename}.txt"));
        let mut file = std::fs::File::create(&path).expect("create sample");
        writeln!(
            file,
            "First paragraph of {basename} talking about distinct prose."
        )
        .unwrap();
        writeln!(
            file,
            "Second paragraph of {basename} continuing the small sample."
        )
        .unwrap();
        writeln!(
            file,
            "Third paragraph of {basename} rounding out the test text."
        )
        .unwrap();
        path
    }

    /// Set up a small library by running the real ingest pipeline once
    /// against `book_count` synthetic plain-text books, embedded with a
    /// dimension-`old_dim` embedder.
    async fn seed_library(
        book_count: usize,
        old_dim: usize,
    ) -> (
        tempfile::TempDir,
        Corpus,
        Catalog,
        std::path::PathBuf,
        Vec<i64>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let lancedb_dir = dir.path().join("lancedb");
        let books_dir = dir.path().join("books");
        std::fs::create_dir_all(&books_dir).expect("books dir");

        let mut intake_ids = Vec::new();
        for i in 0..book_count {
            let basename = format!("book{i}");
            let file = write_sample(dir.path(), &basename);
            let report = ingest_book(
                &file,
                &mut corpus,
                &mut catalog,
                &lancedb_dir,
                &books_dir,
                &Fake {
                    dim: old_dim,
                    tag: 1.0,
                },
                &IngestParams::default(),
            )
            .await
            .expect("ingest book");
            assert!(report.chunks_written > 0, "seed needs non-empty chunks");
            intake_ids.push(report.intake_id);
        }
        (dir, corpus, catalog, lancedb_dir, intake_ids)
    }

    fn embed_cfg(model: &str) -> EmbedConfig {
        EmbedConfig {
            model: model.to_string(),
            ..EmbedConfig::default()
        }
    }

    #[tokio::test]
    async fn reset_swaps_the_index_stamps_and_table_dim() {
        const OLD_DIM: usize = 8;
        const NEW_DIM: usize = 16;
        let (_dir, corpus, catalog, lancedb_dir, intake_ids) = seed_library(2, OLD_DIM).await;

        // Pre-state: old stamps + old-dim chunks + every intake Embedded.
        assert_eq!(
            corpus.meta_get(VECTOR_DIM_KEY).expect("get"),
            Some(OLD_DIM.to_string())
        );

        let cfg = embed_cfg("new-model");
        let report = reset_and_rechunk(
            &catalog,
            &corpus,
            &lancedb_dir,
            &Fake {
                dim: NEW_DIM,
                tag: 9.0,
            },
            &cfg,
            false,
        )
        .await
        .expect("reset");

        assert_eq!(report.intakes_reembedded, intake_ids.len());
        assert!(report.failed_intake.is_none());
        assert!(report.chunks_written > 0);

        // Stamps reflect the new model + new dim.
        assert_eq!(
            corpus.meta_get(EMBED_MODEL_KEY).expect("get"),
            Some("new-model".to_string())
        );
        assert_eq!(
            corpus.meta_get(VECTOR_DIM_KEY).expect("get"),
            Some(NEW_DIM.to_string())
        );

        // The on-disk table is at the new dim.
        let store = ChunkStore::try_open(&lancedb_dir)
            .await
            .expect("try_open")
            .expect("table must exist after reset");
        assert_eq!(store.dimension(), NEW_DIM);

        // Every intake landed back at Embedded.
        for id in intake_ids {
            let intake = catalog
                .intake_by_id(id)
                .expect("by id")
                .expect("intake row");
            assert_eq!(intake.status, IntakeStatus::Embedded);
        }
    }

    #[tokio::test]
    async fn reset_aborts_on_embed_failure_and_resume_finishes_the_run() {
        const OLD_DIM: usize = 8;
        const NEW_DIM: usize = 16;
        let (_dir, corpus, catalog, lancedb_dir, intake_ids) = seed_library(2, OLD_DIM).await;

        let cfg = embed_cfg("new-model");
        let err = reset_and_rechunk(&catalog, &corpus, &lancedb_dir, &Offline, &cfg, false)
            .await
            .expect_err("offline embedder must fail mid-build");
        assert!(
            matches!(err, IngestError::Embed(_)),
            "expected embed error, got {err:?}"
        );

        // After failure: stamps cleared, table dropped, every intake
        // back to Extracted (the first book's embed didn't even probe
        // a dim because the embedder failed before any commit).
        assert_eq!(corpus.meta_get(EMBED_MODEL_KEY).expect("get"), None);
        assert!(
            ChunkStore::try_open(&lancedb_dir)
                .await
                .expect("try_open")
                .is_none()
        );
        for id in &intake_ids {
            let intake = catalog
                .intake_by_id(*id)
                .expect("by id")
                .expect("intake row");
            assert_eq!(intake.status, IntakeStatus::Extracted);
        }

        // The aborted run recorded its embed failure on the trail.
        let failed_books = intake_ids
            .iter()
            .filter(|id| {
                let rows = catalog
                    .pipeline_audit_for_book(PartitionIdx::new(**id).root().get())
                    .expect("trail");
                rows.last()
                    .is_some_and(|r| r.stage == "embed" && r.outcome == "fail")
            })
            .count();
        assert_eq!(
            failed_books, 1,
            "exactly the aborting book records a fail row"
        );

        // Resume with a healthy embedder finishes the work without
        // re-doing the destructive steps.
        let report = reset_and_rechunk(
            &catalog,
            &corpus,
            &lancedb_dir,
            &Fake {
                dim: NEW_DIM,
                tag: 9.0,
            },
            &cfg,
            true,
        )
        .await
        .expect("resume succeeds");
        assert_eq!(report.intakes_reembedded, intake_ids.len());
        assert_eq!(
            corpus.meta_get(VECTOR_DIM_KEY).expect("get"),
            Some(NEW_DIM.to_string())
        );
        for id in intake_ids {
            let intake = catalog
                .intake_by_id(id)
                .expect("by id")
                .expect("intake row");
            assert_eq!(intake.status, IntakeStatus::Embedded);

            // The resume appended an embed-ok row, so the trail's last
            // line agrees with the book's Embedded status.
            let rows = catalog
                .pipeline_audit_for_book(PartitionIdx::new(id).root().get())
                .expect("trail");
            let last = rows.last().expect("trail rows");
            assert_eq!(last.stage, "embed");
            assert_eq!(last.outcome, "ok");
            assert_eq!(last.actor_detail.as_deref(), Some("reset"));
            assert!(last.pipeline_run_id.starts_with("reset-"));
        }
    }

    #[tokio::test]
    async fn resume_on_an_already_clean_library_is_a_noop() {
        const DIM: usize = 8;
        let (_dir, corpus, catalog, lancedb_dir, _ids) = seed_library(1, DIM).await;

        // No `Extracted` intakes — nothing to resume.
        let cfg = embed_cfg("same-model");
        let report = reset_and_rechunk(
            &catalog,
            &corpus,
            &lancedb_dir,
            &Fake { dim: DIM, tag: 1.0 },
            &cfg,
            true,
        )
        .await
        .expect("resume noop");
        assert_eq!(report.intakes_reembedded, 0);
        assert!(report.failed_intake.is_none());
    }
}
