// SPDX-License-Identifier: Apache-2.0

//! The EMBED stage: turn an item's [`ChunkPlan`]s into vectors and
//! write them to the dense store. Books and papers both run it.
//!
//! Batching is sequential and greedy: chunks are packed into a batch up
//! to a character budget and a hard chunk cap, each batch is embedded in
//! one request, and the resulting rows are appended to the store. An
//! overloaded server (HTTP 5xx) is met by halving the batch and retrying
//! each half, down to a single chunk — the producer/consumer overlap and
//! AIMD scheduling the config leaves room for are deferred.
//!
//! A chunk whose text the item's partition already holds a vector for
//! takes that vector instead of a new one, so a run over unchanged text
//! sends no request. A text that several of the item's chunks share is
//! sent once, and every one of them stores the vector that comes back. Writing an item is delete-then-append on its
//! partition, so a run replaces its rows rather than duplicating them;
//! a run that finds every row already in place writes nothing.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::time::{Duration, Instant};

use bookrack_config::EmbedConfig;
use bookrack_core::PartitionIdx;
use bookrack_corpus::Corpus;
use bookrack_embed::Embedder;
use bookrack_text::ChunkPlan;
use bookrack_vectors::{AnnConfig, AnnKind, ChunkReader, ChunkRow, ChunkStore, VectorsError};
use sha2::{Digest, Sha256};

use crate::pipeline::common::stamps::{current_index_identity, current_index_stamps};

bookrack_core::fixed_settings! {
    owner = "ops";
    "ingest.ann_rebuild_floor" = L2_ABSOLUTE_FLOOR,
        "chunks of accumulated churn below which the ANN index is not rebuilt",
        acts on "how often an ingest pays for an index rebuild";
}

/// Why an EMBED run failed. Each pipeline's error type takes it apart
/// into its own variant of the same name.
#[derive(Debug, thiserror::Error)]
pub enum EmbedStageError {
    #[error("embed error")]
    Embed(#[from] bookrack_embed::EmbedError),
    /// The embedder answered the first batch with no vector, or with an
    /// empty one first.
    #[error("the embedder returned no vector for the first batch")]
    EmptyEmbedding,
    #[error("vector store error")]
    Vectors(#[from] bookrack_vectors::VectorsError),
    #[error("corpus error")]
    Corpus(#[from] bookrack_corpus::CorpusError),
}

type Result<T> = std::result::Result<T, EmbedStageError>;

/// What one EMBED run produced — the row count plus the batching metrics
/// that diagnose how the run behaved against the server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbedRunReport {
    /// Rows the item has in the vector store when the run ends. A run
    /// that found every row already in place and wrote none still
    /// counts them all.
    pub chunks_written: usize,
    /// How many of those rows took their vector from the store instead
    /// of from the embedder.
    pub chunks_reused: usize,
    /// Embed requests issued, after any overload shrinking.
    pub batches: usize,
    /// Times an overloaded server forced a batch to be halved.
    pub shrink_events: usize,
    /// Total characters of chunk text sent to the embedder; a text
    /// several chunks share is counted once.
    pub total_chars: usize,
}

/// Where an EMBED run may take a chunk's vector from instead of asking
/// the embedder for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedSource {
    /// The rows the item's own partition holds when the run starts: a
    /// plan whose text one of them carries takes that row's vector.
    SamePartition,
    /// Nowhere: every plan is sent to the embedder.
    Recompute,
}

/// The model and width a set of vectors was produced with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VectorIdentity<'a> {
    pub embed_model: &'a str,
    pub vector_dim: u32,
}

/// Callback an EMBED run invokes as the item's rows come to stand in
/// the store, with `(consumed, total)`: the chunk plans in place so far
/// and the number the run was given. The last call has
/// `consumed == total`.
pub type OnEmbedBatch<'a> = dyn Fn(usize, usize) + Send + Sync + 'a;

/// Embed one item's chunk plans and write them to the store under
/// `lancedb_dir`. Returns the run's [`EmbedRunReport`].
///
/// The index's recorded model and algorithm versions are compared
/// before any request is sent. A plan whose text `source` already holds
/// a vector for takes that vector; the rest are embedded. The first
/// batch's vectors fix the embedding dimension; once they agree with
/// the recorded stamp and the table on disk, the store is opened at
/// that dimension and the item's prior rows are cleared before the
/// first append. The deletion is keyed on the partition the first chunk
/// lives in: every plan of one call belongs to one item.
///
/// A run whose every plan is already stored, row for row, sends no
/// request and leaves the table as it is.
pub async fn embed_chunks<E: Embedder>(
    plans: &[ChunkPlan],
    source: EmbedSource,
    embedder: &E,
    corpus: &Corpus,
    lancedb_dir: &Path,
    cfg: &EmbedConfig,
) -> Result<EmbedRunReport> {
    embed_chunks_with_progress(
        plans,
        source,
        embedder,
        corpus,
        lancedb_dir,
        cfg,
        &|_, _| {},
    )
    .await
}

/// [`embed_chunks`], reporting to `on_batch` as rows come to stand in
/// the store. An empty `plans` writes nothing, reports nothing, and
/// leaves the item's prior rows where they are.
pub async fn embed_chunks_with_progress<E: Embedder>(
    plans: &[ChunkPlan],
    source: EmbedSource,
    embedder: &E,
    corpus: &Corpus,
    lancedb_dir: &Path,
    cfg: &EmbedConfig,
    on_batch: &OnEmbedBatch<'_>,
) -> Result<EmbedRunReport> {
    if plans.is_empty() {
        return Ok(EmbedRunReport::default());
    }

    // The model and the two algorithm versions are compared before the
    // embedder is asked for anything: an item whose build parameters
    // differ from the index's is refused with no request sent and
    // nothing opened.
    let recorded_dim = corpus.precheck_index_stamps(&current_index_identity(&cfg.model))?;
    let partition = plans[0].start_node_id.partition();
    let total = plans.len();

    // The item's stored rows are a source of vectors only on a stamped
    // index: the stamp is the one record of which model produced them.
    let mut table_dim: Option<usize> = None;
    let mut table_looked_at = false;
    let mut source_rows: Vec<ChunkRow> = Vec::new();
    let mut reused: Vec<Option<Vec<f32>>> = vec![None; total];
    if source == EmbedSource::SamePartition
        && let Some(vector_dim) = recorded_dim
    {
        table_looked_at = true;
        if let Some(reader) = ChunkReader::open(lancedb_dir).await? {
            table_dim = Some(reader.dimension());
            source_rows = reader.scan_partition(partition).await?;
            let identity = VectorIdentity {
                embed_model: &cfg.model,
                vector_dim,
            };
            reused = reuse_vectors(Some((&identity, &source_rows)), &identity, plans);
        }
    }
    let mut reused_rows: Vec<ChunkRow> = Vec::new();
    let mut unstored: Vec<&ChunkPlan> = Vec::new();
    for (plan, vector) in plans.iter().zip(reused) {
        match vector {
            Some(vector) => reused_rows.push(row_of(plan, vector)),
            None => unstored.push(plan),
        }
    }
    // A text several of the item's chunks share is sent once. Batches
    // are cut over the texts to send, not over the chunks.
    let (to_embed, repeats) = fold_repeated_texts(unstored);
    tracing::info!(
        reused = reused_rows.len(),
        to_embed = to_embed.len(),
        repeated = repeats.iter().map(Vec::len).sum::<usize>(),
        total,
        "embed stage entered"
    );
    let mut report = EmbedRunReport {
        chunks_reused: reused_rows.len(),
        ..EmbedRunReport::default()
    };
    let slice_len = cfg.batch_max_chunks.max(1);

    // Every plan has a stored vector: no request is sent. The reused
    // vectors all have the recorded width, so the first one names it.
    if to_embed.is_empty() {
        let dim = reused_rows[0].vector.len();
        let unchanged = rows_unchanged(&source_rows, plans, dim);
        if unchanged && bookrack_vectors::meta::load(lancedb_dir)?.is_some() {
            tracing::info!(
                total,
                "every chunk is stored as planned; the table is left as it is"
            );
            report.chunks_written = total;
            on_batch(total, total);
            return Ok(report);
        }
        let store = ChunkStore::open_for_write(lancedb_dir, dim).await?;
        if store.dimension() != dim {
            return Err(VectorsError::DimensionMismatch {
                got: dim,
                expected: store.dimension(),
            }
            .into());
        }
        let mut churn = 0usize;
        if unchanged {
            // The rows stand; only the index declaration is missing.
            report.chunks_written = total;
            on_batch(total, total);
        } else {
            let deleted = clear_partition(&store, partition).await?;
            let mut consumed = 0usize;
            append_in_slices(
                &store,
                &reused_rows,
                slice_len,
                &mut consumed,
                total,
                on_batch,
            )
            .await?;
            report.chunks_written = consumed;
            churn = deleted + consumed;
        }
        finish_write(&store, lancedb_dir, churn as u64).await;
        return Ok(report);
    }

    // The first batch fixes the vector width, and is held in memory
    // until it has passed every check below.
    let first_end = greedy_batch_end(&to_embed, 0, cfg);
    let (first_vectors, first_shrinks) =
        embed_with_shrink(embedder, &to_embed[..first_end], 0).await?;
    let dim = first_vectors
        .first()
        .map(Vec::len)
        .filter(|&d| d > 0)
        .ok_or(EmbedStageError::EmptyEmbedding)?;

    // Three read-only checks, in a fixed order, ahead of the first
    // write: the width against the recorded stamp, against the table on
    // disk, and against every other vector of the batch. A refusal here
    // has stamped nothing and deleted nothing. The chunks table and
    // `vectors_meta.json`, which describes that table, stand; the file
    // goes only when the table does, in `drop_chunks_table`.
    if let Some(recorded) = recorded_dim
        && recorded as usize != dim
    {
        return Err(bookrack_corpus::CorpusError::IndexStampMismatch {
            key: bookrack_corpus::VECTOR_DIM_KEY,
            found: recorded.to_string(),
            expected: dim.to_string(),
        }
        .into());
    }
    if !table_looked_at {
        table_dim = ChunkReader::open(lancedb_dir)
            .await?
            .map(|reader| reader.dimension());
    }
    if let Some(on_disk) = table_dim
        && on_disk != dim
    {
        return Err(VectorsError::DimensionMismatch {
            got: dim,
            expected: on_disk,
        }
        .into());
    }
    expect_one_vector_each(first_end, &first_vectors)?;
    for (row, vector) in first_vectors.iter().enumerate() {
        if vector.len() != dim {
            return Err(VectorsError::DimensionMismatch {
                got: vector.len(),
                expected: dim,
            }
            .into());
        }
        if let Some(defect) = bookrack_core::vector_defect(vector) {
            return Err(VectorsError::UnusableVector { row, defect }.into());
        }
    }

    // Stamp the build parameters on a fresh index, before any vector is
    // written, so the index is never mixed.
    if recorded_dim.is_none() {
        corpus.reconcile_index_stamps(&current_index_stamps(&cfg.model, dim as u32))?;
    }

    let store = ChunkStore::open_for_write(lancedb_dir, dim).await?;
    let deleted = clear_partition(&store, partition).await?;

    // The reused rows go back first, then each embedded batch as it
    // arrives.
    let mut consumed = 0usize;
    append_in_slices(
        &store,
        &reused_rows,
        slice_len,
        &mut consumed,
        total,
        on_batch,
    )
    .await?;

    let mut start = 0usize;
    let phase_started = Instant::now();
    let mut ticker = ProgressTicker::new(cfg.progress_interval);
    let mut held = Some((first_end, first_vectors, first_shrinks));
    while start < to_embed.len() {
        let (end, vectors, shrinks) = match held.take() {
            Some(first) => first,
            None => {
                let end = greedy_batch_end(&to_embed, start, cfg);
                let started = Instant::now();
                let (vectors, shrinks) =
                    embed_with_shrink(embedder, &to_embed[start..end], 0).await?;
                tracing::debug!(
                    vectors = vectors.len(),
                    elapsed_ms = started.elapsed().as_secs_f64() * 1e3,
                    "batch embedded"
                );
                expect_one_vector_each(end - start, &vectors)?;
                (end, vectors, shrinks)
            }
        };
        let batch = &to_embed[start..end];
        let batch_chars: usize = batch.iter().map(|p| p.text.chars().count()).sum();

        let rows = store
            .append(&rows_with_repeats(batch, &repeats[start..end], vectors))
            .await?;
        tracing::debug!(rows, chars = batch_chars, "appended batch to store");

        consumed += rows;
        report.batches += 1;
        report.shrink_events += shrinks;
        report.total_chars += batch_chars;
        start = end;
        on_batch(consumed, total);

        if ticker.should_emit(Instant::now()) {
            tracing::info!(
                chunks_done = consumed,
                chunks_total = total,
                batch = report.batches,
                elapsed_s = phase_started.elapsed().as_secs_f64(),
                "embed progress",
            );
        }
    }
    report.chunks_written = consumed;

    finish_write(&store, lancedb_dir, (deleted + consumed) as u64).await;
    Ok(report)
}

/// For each plan, the stored vector it may take, or `None` when it has
/// to be embedded.
///
/// A plan takes the vector of a `source` row whose text is byte-equal
/// to its own, compared by the SHA-256 of the text as it is sent to the
/// embedder. Nothing is taken when there is no source, or when the
/// source's model or width differs from `target`'s. A source row whose
/// vector is not `source`'s width, or is unusable for cosine search, is
/// passed over. Of several rows carrying one text, the one first by
/// chunk coordinates gives its vector, whatever order the rows arrive
/// in.
pub(crate) fn reuse_vectors(
    source: Option<(&VectorIdentity<'_>, &[ChunkRow])>,
    target: &VectorIdentity<'_>,
    plans: &[ChunkPlan],
) -> Vec<Option<Vec<f32>>> {
    let Some((identity, rows)) = source.filter(|(identity, _)| *identity == target) else {
        return vec![None; plans.len()];
    };
    let width = identity.vector_dim as usize;
    let mut usable: Vec<&ChunkRow> = rows
        .iter()
        .filter(|row| vector_is_usable(&row.vector, width))
        .collect();
    usable.sort_by_key(|row| row_coordinates(row));
    let mut by_text: HashMap<[u8; 32], &[f32]> = HashMap::new();
    for row in usable {
        by_text
            .entry(sent_text_digest(&row.text))
            .or_insert(&row.vector);
    }
    plans
        .iter()
        .map(|plan| {
            by_text
                .get(&sent_text_digest(&plan.text))
                .map(|vector| vector.to_vec())
        })
        .collect()
}

/// Whether the stored rows are exactly the rows the plans would write:
/// the same count, and, paired by chunk coordinates, the same text and
/// normalized-text hash. Any stored vector that is not `expected_dim`
/// wide or is unusable for cosine search makes the answer `false`, so a
/// defective row is rewritten instead of left standing.
pub(crate) fn rows_unchanged(
    source_rows: &[ChunkRow],
    plans: &[ChunkPlan],
    expected_dim: usize,
) -> bool {
    if source_rows.len() != plans.len()
        || source_rows
            .iter()
            .any(|row| !vector_is_usable(&row.vector, expected_dim))
    {
        return false;
    }
    let mut rows: Vec<&ChunkRow> = source_rows.iter().collect();
    rows.sort_by_key(|row| row_coordinates(row));
    let mut planned: Vec<&ChunkPlan> = plans.iter().collect();
    planned.sort_by_key(|plan| plan_coordinates(plan));
    rows.iter().zip(planned).all(|(row, plan)| {
        row_coordinates(row) == plan_coordinates(plan)
            && row.text == plan.text
            && row.norm_chunk_sha256 == plan.norm_chunk_sha256
    })
}

/// Where a chunk starts and ends: node and character offset, twice.
type Coordinates = (i64, i32, i64, i32);

fn row_coordinates(row: &ChunkRow) -> Coordinates {
    (
        row.start_node_id.get(),
        row.start_char_offset,
        row.end_node_id.get(),
        row.end_char_offset,
    )
}

fn plan_coordinates(plan: &ChunkPlan) -> Coordinates {
    (
        plan.start_node_id.get(),
        plan.start_char_offset,
        plan.end_node_id.get(),
        plan.end_char_offset,
    )
}

/// Whether a stored vector is `width` wide and usable for cosine search.
fn vector_is_usable(vector: &[f32], width: usize) -> bool {
    vector.len() == width && bookrack_core::vector_defect(vector).is_none()
}

/// SHA-256 of a chunk's text as the embedder is sent it.
fn sent_text_digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

/// Delete every row of `partition`, returning how many there were.
async fn clear_partition(store: &ChunkStore, partition: PartitionIdx) -> Result<usize> {
    let before = store.count_rows().await?;
    store.delete_partition(partition).await?;
    let after = store.count_rows().await?;
    Ok(before.saturating_sub(after))
}

/// Append `rows` in slices of `slice_len`, adding each slice's rows to
/// `consumed` and reporting it once the slice has landed.
async fn append_in_slices(
    store: &ChunkStore,
    rows: &[ChunkRow],
    slice_len: usize,
    consumed: &mut usize,
    total: usize,
    on_batch: &OnEmbedBatch<'_>,
) -> Result<()> {
    for slice in rows.chunks(slice_len) {
        *consumed += store.append(slice).await?;
        on_batch(*consumed, total);
    }
    Ok(())
}

/// Close a run that opened the store for writing: compact, account for
/// `churn_delta` rows of delete-and-append churn, and build or retrain
/// the ANN index when it is due. Every step is best-effort.
async fn finish_write(store: &ChunkStore, lancedb_dir: &Path, churn_delta: u64) {
    // Drain the delete-then-append churn: compact fragments, prune old
    // versions, and absorb the freshly-appended rows into any existing
    // index. Failure is non-fatal — the table is still consistent, just
    // carrying tombstones and unindexed rows the next run will pick up.
    let optimize_ok = match store.optimize().await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = ?e, "lancedb optimize failed; continuing");
            false
        }
    };

    // Bump the churn counter in vectors_meta.json if a meta file is
    // present. Skip when none — that's the "ANN not yet adopted" state,
    // handled by the cold-start path below. Failures here are
    // non-fatal: the next run will recompute against the new state.
    if churn_delta > 0
        && let Err(e) = bump_churn(lancedb_dir, churn_delta)
    {
        tracing::warn!(error = ?e, "failed to update vectors_meta churn counter; continuing");
    }

    // Cold-start build / retrain on churn threshold. The L1 optimize
    // above is the pre-step the retrain order requires (compaction
    // first so that tombstones do not flow into the kmeans training
    // set); skip the rebuild when optimize failed so tombstones do not
    // poison the kmeans training set, and let the next run retry.
    if optimize_ok {
        if let Err(e) = maybe_build_ann_index(store, lancedb_dir).await {
            tracing::warn!(error = ?e, "ann build/retrain failed; will retry next run");
        }
    } else {
        tracing::warn!("skipping ann build/retrain after optimize failure; will retry next run");
    }
}

/// Add `delta` to `vectors_meta::churn_since_rebuild` if a meta file is
/// present. Absent meta is a no-op (returns Ok) — the cold-start path
/// owns the first build.
fn bump_churn(lancedb_dir: &Path, delta: u64) -> Result<()> {
    let Some(mut meta) =
        bookrack_vectors::meta::load(lancedb_dir).map_err(EmbedStageError::Vectors)?
    else {
        return Ok(());
    };
    meta.churn_since_rebuild = meta.churn_since_rebuild.saturating_add(delta);
    bookrack_vectors::meta::store(lancedb_dir, &meta).map_err(EmbedStageError::Vectors)?;
    Ok(())
}

/// L2 retrain threshold: rebuild the ANN index when accumulated churn
/// has reached either twice the size of the corpus at the last build or
/// the absolute floor of 20,000 chunks, whichever is larger. The floor
/// prevents trigger-happy retraining on small libraries.
// setting: ingest.ann_rebuild_floor
pub(crate) const L2_ABSOLUTE_FLOOR: u64 = 20_000;

/// Build the ANN index if it should be: either the cold-start case
/// (no meta yet) or the L2 churn-threshold case.
async fn maybe_build_ann_index(store: &ChunkStore, lancedb_dir: &Path) -> Result<()> {
    let template = match bookrack_vectors::meta::load(lancedb_dir)
        .map_err(EmbedStageError::Vectors)?
    {
        None => {
            // Cold start: no meta file. Build the C1-recommended default.
            AnnConfig::default_for(AnnKind::IvfFlat)
        }
        Some(meta) => {
            // BruteForce kind means a previous drop_ann_index set it
            // explicitly. Respect that — don't auto-rebuild.
            let parsed = AnnConfig::from_meta(&meta).map_err(EmbedStageError::Vectors)?;
            if parsed.kind == AnnKind::BruteForce {
                return Ok(());
            }
            let threshold = (meta.built_at_chunk_count.saturating_mul(2)).max(L2_ABSOLUTE_FLOOR);
            if meta.churn_since_rebuild < threshold {
                return Ok(());
            }
            tracing::info!(
                churn = meta.churn_since_rebuild,
                threshold,
                kind = parsed.kind.as_str(),
                "ann churn threshold crossed; retraining"
            );
            parsed
        }
    };
    let chunk_count = store.count_rows().await.map_err(EmbedStageError::Vectors)? as u64;
    let cfg = scale_to_corpus(template, chunk_count);
    store
        .build_ann_index(&cfg, lancedb_dir, now_rfc3339())
        .await
        .map_err(EmbedStageError::Vectors)?;
    Ok(())
}

/// LanceDB's IVF trainer needs roughly `sample_rate × num_partitions`
/// training vectors (default `sample_rate = 256`). Clamp the requested
/// `num_partitions` so the corpus has enough rows to train them — a
/// brand-new library doing its first small ingest stays at one
/// partition, and grows to the requested value naturally as more
/// books arrive.
fn scale_to_corpus(mut cfg: AnnConfig, chunk_count: u64) -> AnnConfig {
    if cfg.kind == AnnKind::BruteForce {
        return cfg;
    }
    let max_partitions = u32::try_from(chunk_count / 256).unwrap_or(u32::MAX).max(1);
    if cfg.num_partitions > max_partitions {
        cfg.num_partitions = max_partitions;
    }
    cfg
}

/// Current wall-clock time as an RFC 3339 UTC string at second
/// precision, e.g. `2024-06-05T07:40:00Z`.
// Parallel implementation: rfc3339-now.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Throttle for the EMBED-progress heartbeat: emits the first tick
/// unconditionally so users see a sign of life right away, then once
/// per `interval`. `interval == Duration::ZERO` emits on every batch.
#[derive(Debug)]
struct ProgressTicker {
    interval: Duration,
    last: Option<Instant>,
}

impl ProgressTicker {
    fn new(interval: Duration) -> Self {
        ProgressTicker {
            interval,
            last: None,
        }
    }

    /// True iff the loop should emit a heartbeat at `now`. Records the
    /// emission timestamp on `true` so the next call respects the
    /// interval.
    fn should_emit(&mut self, now: Instant) -> bool {
        match self.last {
            None => {
                self.last = Some(now);
                true
            }
            Some(last) if now.duration_since(last) >= self.interval => {
                self.last = Some(now);
                true
            }
            Some(_) => false,
        }
    }
}

/// Find the end of the greedy batch starting at `start`: grow while the
/// running character count stays under the budget and the chunk count
/// stays under the cap, but always take at least one chunk so an
/// over-budget chunk still makes progress.
fn greedy_batch_end(plans: &[&ChunkPlan], start: usize, cfg: &EmbedConfig) -> usize {
    let mut end = start;
    let mut chars = 0usize;
    while end < plans.len() {
        let len = plans[end].text.chars().count();
        if end > start
            && (end - start >= cfg.batch_max_chunks || chars + len > cfg.batch_char_budget)
        {
            break;
        }
        chars += len;
        end += 1;
    }
    end
}

/// Embed one batch, halving and retrying each half on overload until it
/// succeeds or reaches a single chunk. A single chunk cannot shrink
/// further, so an overload there surfaces as an error. `depth` is the
/// recursion depth, recorded on the shrink event so a deep shrink cascade
/// is visible in the logs.
///
/// Returns the embedding vectors and the number of shrink events the call
/// triggered — one per overload that forced a halving, summed across the
/// recursion.
async fn embed_with_shrink<E: Embedder>(
    embedder: &E,
    batch: &[&ChunkPlan],
    depth: usize,
) -> Result<(Vec<Vec<f32>>, usize)> {
    let texts: Vec<String> = batch.iter().map(|p| p.text.clone()).collect();
    match embedder.embed_batch(&texts).await {
        Ok(vectors) => Ok((vectors, 0)),
        Err(e) if e.is_overload() && batch.len() > 1 => {
            let mid = batch.len() / 2;
            tracing::warn!(
                before = batch.len(),
                after = mid,
                depth,
                "embed server overloaded; shrinking batch"
            );
            let (mut left, left_shrinks) =
                Box::pin(embed_with_shrink(embedder, &batch[..mid], depth + 1)).await?;
            let (right, right_shrinks) =
                Box::pin(embed_with_shrink(embedder, &batch[mid..], depth + 1)).await?;
            left.extend(right);
            Ok((left, 1 + left_shrinks + right_shrinks))
        }
        Err(e) => Err(EmbedStageError::Embed(e)),
    }
}

/// Split `plans` into the first plan of each distinct text, in order,
/// and beside each one the later plans carrying that same text. Texts
/// are compared as [`reuse_vectors`] compares them.
fn fold_repeated_texts(plans: Vec<&ChunkPlan>) -> (Vec<&ChunkPlan>, Vec<Vec<&ChunkPlan>>) {
    let mut first_of: HashMap<[u8; 32], usize> = HashMap::new();
    let mut firsts: Vec<&ChunkPlan> = Vec::new();
    let mut repeats: Vec<Vec<&ChunkPlan>> = Vec::new();
    for plan in plans {
        match first_of.entry(sent_text_digest(&plan.text)) {
            Entry::Occupied(seen) => repeats[*seen.get()].push(plan),
            Entry::Vacant(unseen) => {
                unseen.insert(firsts.len());
                firsts.push(plan);
                repeats.push(Vec::new());
            }
        }
    }
    (firsts, repeats)
}

/// Refuse an answer that does not carry one vector per text sent. The
/// embed client checks this itself; another [`Embedder`] may not, and
/// [`rows_with_repeats`] pairs vectors with plans by position.
fn expect_one_vector_each(sent: usize, vectors: &[Vec<f32>]) -> Result<()> {
    if vectors.len() == sent {
        return Ok(());
    }
    Err(EmbedStageError::Embed(
        bookrack_embed::EmbedError::MalformedResponse(format!(
            "got {} vectors for {} inputs",
            vectors.len(),
            sent
        )),
    ))
}

/// Zip a batch of plans with its embedding vectors into store rows, one
/// vector per plan, in order. `repeats` holds, beside each plan, the
/// plans that carry its text again: each gets a row with a copy of the
/// same vector.
fn rows_with_repeats(
    plans: &[&ChunkPlan],
    repeats: &[Vec<&ChunkPlan>],
    vectors: Vec<Vec<f32>>,
) -> Vec<ChunkRow> {
    let mut rows = Vec::with_capacity(plans.len() + repeats.iter().map(Vec::len).sum::<usize>());
    for ((plan, again), vector) in plans.iter().zip(repeats).zip(vectors) {
        for repeat in again {
            rows.push(row_of(repeat, vector.clone()));
        }
        rows.push(row_of(plan, vector));
    }
    rows
}

/// The store row of one plan carrying `vector`.
fn row_of(plan: &ChunkPlan, vector: Vec<f32>) -> ChunkRow {
    ChunkRow {
        vector,
        text: plan.text.clone(),
        start_node_id: plan.start_node_id,
        start_char_offset: plan.start_char_offset,
        end_node_id: plan.end_node_id,
        end_char_offset: plan.end_char_offset,
        norm_chunk_sha256: plan.norm_chunk_sha256.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_core::NodeId;
    use bookrack_corpus::CorpusError;
    use bookrack_embed::{EmbedError, Result as EmbedResult};
    use std::future::Future;

    /// A chunk plan in partition 1 at the given local offset.
    fn plan(offset: i64, text: &str) -> ChunkPlan {
        let node = NodeId::new(100_000_000 + offset);
        ChunkPlan {
            start_node_id: node,
            start_char_offset: 0,
            end_node_id: node,
            end_char_offset: text.chars().count() as i32,
            text: text.to_string(),
            norm_chunk_sha256: bookrack_text::norm_text_sha256(text),
        }
    }

    /// A one-character text that differs for each `offset` from 1 to 26.
    fn letter(offset: i64) -> String {
        let step = u8::try_from(offset - 1).expect("an offset from 1 to 26");
        char::from(b'a' + step).to_string()
    }

    /// A fake embedder returning constant `dim`-length vectors.
    struct Fake {
        dim: usize,
    }

    impl Embedder for Fake {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let dim = self.dim;
            let n = texts.len();
            async move { Ok(vec![vec![0.5f32; dim]; n]) }
        }
    }

    /// A fake that reports overload for any batch larger than `max_batch`,
    /// so the shrink path can be exercised offline.
    struct Overloading {
        dim: usize,
        max_batch: usize,
    }

    impl Embedder for Overloading {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let dim = self.dim;
            let n = texts.len();
            let max = self.max_batch;
            async move {
                if n > max {
                    Err(EmbedError::Overloaded {
                        status: 500,
                        body: String::new(),
                    })
                } else {
                    Ok(vec![vec![0.5f32; dim]; n])
                }
            }
        }
    }

    /// A fake that counts the requests it receives and answers each from
    /// `answer(call, texts)`, where `call` counts from zero.
    struct Scripted {
        calls: std::sync::atomic::AtomicUsize,
        sent: std::sync::Mutex<Vec<String>>,
        answer: Box<Answer>,
    }

    type Answer = dyn Fn(usize, &[String]) -> EmbedResult<Vec<Vec<f32>>> + Send + Sync;

    impl Scripted {
        fn new(
            answer: impl Fn(usize, &[String]) -> EmbedResult<Vec<Vec<f32>>> + Send + Sync + 'static,
        ) -> Self {
            Scripted {
                calls: std::sync::atomic::AtomicUsize::new(0),
                sent: std::sync::Mutex::new(Vec::new()),
                answer: Box::new(answer),
            }
        }

        /// Every text received so far, in the order it was sent.
        fn sent(&self) -> Vec<String> {
            self.sent.lock().expect("sent").clone()
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Embedder for Scripted {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.sent.lock().expect("sent").extend_from_slice(texts);
            let answer = (self.answer)(call, texts);
            async move { answer }
        }
    }

    /// A [`Scripted`] fake answering every text with a constant
    /// `dim`-length vector.
    fn counting(dim: usize) -> Scripted {
        Scripted::new(move |_, texts: &[String]| Ok(vec![vec![0.5f32; dim]; texts.len()]))
    }

    /// The rows stored for partition 1, or zero when no table exists.
    async fn partition_rows(lancedb_dir: &Path) -> usize {
        match ChunkReader::open(lancedb_dir).await.expect("open reader") {
            Some(reader) => reader
                .scan_partition(plan(1, "").start_node_id.partition())
                .await
                .expect("scan")
                .len(),
            None => 0,
        }
    }

    /// Every path under `dir`, sorted.
    fn tree(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in std::fs::read_dir(&next).expect("read dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path.clone());
                }
                found.push(path);
            }
        }
        found.sort();
        found
    }

    /// Create the chunks table at `dim` and store one row in partition 1.
    async fn seed_table(lancedb_dir: &Path, dim: usize) {
        let store = ChunkStore::open_for_write(lancedb_dir, dim)
            .await
            .expect("create table");
        store
            .append(&[row_of(&plan(1, "seed"), vec![0.5f32; dim])])
            .await
            .expect("seed row");
    }

    #[tokio::test]
    async fn a_different_model_is_refused_without_a_request_or_a_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, &format!("prose {i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let first = EmbedConfig {
            model: "model-a".to_string(),
            ..EmbedConfig::default()
        };
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &first,
        )
        .await
        .expect("first run");
        let before = tree(dir.path());

        let second = EmbedConfig {
            model: "model-b".to_string(),
            ..EmbedConfig::default()
        };
        let embedder = counting(8);
        let err = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &embedder,
            &corpus,
            dir.path(),
            &second,
        )
        .await
        .expect_err("a different model must be refused");
        assert!(matches!(
            err,
            EmbedStageError::Corpus(CorpusError::IndexStampMismatch { key, .. })
                if key == bookrack_corpus::EMBED_MODEL_KEY
        ));
        assert_eq!(embedder.calls(), 0, "the refusal precedes any request");
        assert_eq!(tree(dir.path()), before, "the refusal creates nothing");
    }

    #[tokio::test]
    async fn one_chunk_is_embedded_in_one_request() {
        let dir = tempfile::tempdir().expect("temp dir");
        let corpus = Corpus::open_in_memory().expect("corpus");
        let embedder = counting(8);
        let report = embed_chunks(
            &[plan(1, "prose")],
            EmbedSource::SamePartition,
            &embedder,
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(report.chunks_written, 1);
        assert_eq!(embedder.calls(), 1);
    }

    #[tokio::test]
    async fn an_unstamped_index_whose_table_has_another_width_is_refused_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        seed_table(dir.path(), 4).await;
        let corpus = Corpus::open_in_memory().expect("corpus");
        let err = embed_chunks(
            &[plan(1, "prose")],
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect_err("a table of another width must refuse the run");
        assert!(
            matches!(
                err,
                EmbedStageError::Vectors(VectorsError::DimensionMismatch {
                    got: 8,
                    expected: 4
                })
            ),
            "{err:?}"
        );
        assert_eq!(
            corpus
                .meta_get(bookrack_corpus::EMBED_MODEL_KEY)
                .expect("get"),
            None,
            "a refused run must not stamp the index",
        );
        assert_eq!(partition_rows(dir.path()).await, 1, "the stored row stands");
    }

    #[tokio::test]
    async fn a_stamped_index_whose_table_has_another_width_is_refused_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        seed_table(dir.path(), 4).await;
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        corpus
            .reconcile_index_stamps(&current_index_stamps(&cfg.model, 8))
            .expect("stamp");
        let err = embed_chunks(
            &[plan(1, "prose")],
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect_err("a table of another width must refuse the run");
        assert!(
            matches!(
                err,
                EmbedStageError::Vectors(VectorsError::DimensionMismatch {
                    got: 8,
                    expected: 4
                })
            ),
            "{err:?}"
        );
        assert_eq!(partition_rows(dir.path()).await, 1, "the stored row stands");
    }

    /// Embed three plans at width 8, then run them again against
    /// `second`; returns the second run's error and the rows left.
    async fn rerun_against<E: Embedder>(second: &E) -> (EmbedStageError, usize) {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, &format!("prose {i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect("first run");
        let err = embed_chunks(
            &plans,
            EmbedSource::Recompute,
            second,
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect_err("the second run must be refused");
        (err, partition_rows(dir.path()).await)
    }

    #[tokio::test]
    async fn a_first_batch_with_one_vector_of_another_width_deletes_nothing() {
        let embedder = Scripted::new(|_, texts: &[String]| {
            let mut vectors = vec![vec![0.5f32; 8]; texts.len()];
            if let Some(second) = vectors.get_mut(1) {
                second.truncate(5);
            }
            Ok(vectors)
        });
        let (err, rows) = rerun_against(&embedder).await;
        assert!(
            matches!(
                err,
                EmbedStageError::Vectors(VectorsError::DimensionMismatch {
                    got: 5,
                    expected: 8
                })
            ),
            "{err:?}"
        );
        assert_eq!(rows, 3, "the item's stored rows stand");
    }

    #[tokio::test]
    async fn a_first_batch_with_a_zero_vector_deletes_nothing() {
        let embedder = Scripted::new(|_, texts: &[String]| {
            let mut vectors = vec![vec![0.5f32; 8]; texts.len()];
            if let Some(second) = vectors.get_mut(1) {
                second.fill(0.0);
            }
            Ok(vectors)
        });
        let (err, rows) = rerun_against(&embedder).await;
        assert!(
            matches!(
                err,
                EmbedStageError::Vectors(VectorsError::UnusableVector { row: 1, .. })
            ),
            "{err:?}"
        );
        assert_eq!(rows, 3, "the item's stored rows stand");
    }

    /// A [`Scripted`] fake that answers the request numbered `short_call`
    /// with one vector fewer than it was sent, when it was sent several.
    fn one_vector_short_on(short_call: usize) -> Scripted {
        Scripted::new(move |call, texts: &[String]| {
            let short = call == short_call && texts.len() > 1;
            Ok(vec![vec![0.5f32; 8]; texts.len() - usize::from(short)])
        })
    }

    #[tokio::test]
    async fn a_first_batch_answered_with_too_few_vectors_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, &format!("prose {i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let embedder = one_vector_short_on(0);
        let err = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &embedder,
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect_err("a short answer must not be zipped onto the plans");
        assert!(
            matches!(
                err,
                EmbedStageError::Embed(EmbedError::MalformedResponse(_))
            ),
            "{err:?}"
        );
        assert_eq!(partition_rows(dir.path()).await, 0);
    }

    #[tokio::test]
    async fn a_later_batch_answered_with_too_few_vectors_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=4).map(|i| plan(i, &format!("prose {i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig {
            batch_max_chunks: 2,
            ..EmbedConfig::default()
        };
        // Request 0 is the first batch of two, request 1 the second.
        let embedder = one_vector_short_on(1);
        let err = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &embedder,
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect_err("a short answer must not be zipped onto the plans");
        assert!(
            matches!(
                err,
                EmbedStageError::Embed(EmbedError::MalformedResponse(_))
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_first_batch_leaves_a_fresh_library_unstamped_and_without_a_table() {
        let dir = tempfile::tempdir().expect("temp dir");
        let lancedb_dir = dir.path().join("lancedb");
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, &format!("prose {i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let embedder = Scripted::new(|_, texts: &[String]| {
            let mut vectors = vec![vec![0.5f32; 8]; texts.len()];
            if let Some(second) = vectors.get_mut(1) {
                second.fill(0.0);
            }
            Ok(vectors)
        });
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &embedder,
            &corpus,
            &lancedb_dir,
            &EmbedConfig::default(),
        )
        .await
        .expect_err("a zero vector must refuse the run");
        assert_eq!(
            corpus
                .meta_get(bookrack_corpus::EMBED_MODEL_KEY)
                .expect("get"),
            None,
            "a refused run must not stamp the index",
        );
        assert!(
            ChunkReader::open(&lancedb_dir)
                .await
                .expect("open reader")
                .is_none(),
            "a refused run must not create the table",
        );
    }

    /// Three plans of distinct text in partition 1.
    fn three_plans() -> Vec<ChunkPlan> {
        (1..=3).map(|i| plan(i, &format!("prose {i}"))).collect()
    }

    /// The rows stored for partition 1, ordered by chunk coordinates.
    async fn stored_rows(lancedb_dir: &Path) -> Vec<ChunkRow> {
        let reader = ChunkReader::open(lancedb_dir)
            .await
            .expect("open reader")
            .expect("a table");
        let mut rows = reader
            .scan_partition(plan(1, "").start_node_id.partition())
            .await
            .expect("scan");
        rows.sort_by_key(row_coordinates);
        rows
    }

    #[tokio::test]
    async fn a_second_run_over_the_same_plans_sends_no_request() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans = three_plans();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        let embedder = counting(8);
        let source = EmbedSource::SamePartition;
        embed_chunks(&plans, source, &embedder, &corpus, dir.path(), &cfg)
            .await
            .expect("first run");
        let calls_after_first = embedder.calls();

        let second = embed_chunks(&plans, source, &embedder, &corpus, dir.path(), &cfg)
            .await
            .expect("second run");
        assert_eq!(
            embedder.calls(),
            calls_after_first,
            "every vector is stored"
        );
        assert_eq!(
            second,
            EmbedRunReport {
                chunks_written: 3,
                chunks_reused: 3,
                batches: 0,
                shrink_events: 0,
                total_chars: 0,
            }
        );
        assert_eq!(partition_rows(dir.path()).await, 3);
    }

    #[tokio::test]
    async fn only_the_plan_whose_text_changed_is_sent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut plans = three_plans();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        let source = EmbedSource::SamePartition;
        embed_chunks(&plans, source, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("first run");

        plans[1] = plan(2, "prose rewritten");
        let embedder = counting(8);
        let second = embed_chunks(&plans, source, &embedder, &corpus, dir.path(), &cfg)
            .await
            .expect("second run");
        assert_eq!(embedder.sent(), ["prose rewritten"]);
        assert_eq!(
            second,
            EmbedRunReport {
                chunks_written: 3,
                chunks_reused: 2,
                batches: 1,
                shrink_events: 0,
                total_chars: "prose rewritten".len(),
            }
        );
        let texts: Vec<String> = stored_rows(dir.path())
            .await
            .into_iter()
            .map(|row| row.text)
            .collect();
        assert_eq!(texts, ["prose 1", "prose rewritten", "prose 3"]);
    }

    #[tokio::test]
    async fn a_first_batch_of_another_width_leaves_the_reusable_rows_in_place() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut plans = three_plans();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        let source = EmbedSource::SamePartition;
        embed_chunks(&plans, source, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("first run");

        plans[1] = plan(2, "prose rewritten");
        let err = embed_chunks(&plans, source, &Fake { dim: 5 }, &corpus, dir.path(), &cfg)
            .await
            .expect_err("a vector of another width must be refused");
        assert!(
            matches!(
                err,
                EmbedStageError::Corpus(CorpusError::IndexStampMismatch { key, .. })
                    if key == bookrack_corpus::VECTOR_DIM_KEY
            ),
            "{err:?}"
        );
        let texts: Vec<String> = stored_rows(dir.path())
            .await
            .into_iter()
            .map(|row| row.text)
            .collect();
        assert_eq!(texts, ["prose 1", "prose 2", "prose 3"]);
    }

    #[tokio::test]
    async fn a_run_that_finds_its_rows_in_place_adds_no_churn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans = three_plans();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        let source = EmbedSource::SamePartition;
        embed_chunks(&plans, source, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("first run");
        let meta_path = dir.path().join("vectors_meta.json");
        let before = std::fs::read(&meta_path).expect("the first run wrote the file");

        embed_chunks(&plans, source, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("second run");
        assert_eq!(std::fs::read(&meta_path).expect("meta bytes"), before);
    }

    #[tokio::test]
    async fn a_run_that_finds_its_rows_in_place_rebuilds_a_missing_index_declaration() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans = three_plans();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        let source = EmbedSource::SamePartition;
        embed_chunks(&plans, source, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("first run");
        let meta_path = dir.path().join("vectors_meta.json");
        std::fs::remove_file(&meta_path).expect("remove the declaration");

        let embedder = counting(8);
        let second = embed_chunks(&plans, source, &embedder, &corpus, dir.path(), &cfg)
            .await
            .expect("second run");
        assert_eq!(embedder.calls(), 0);
        assert_eq!(second.chunks_reused, 3);
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("the declaration is back");
        assert_eq!(meta.built_at_chunk_count, 3);
        assert_eq!(meta.churn_since_rebuild, 0);
        assert_eq!(partition_rows(dir.path()).await, 3);
    }

    #[tokio::test]
    async fn recompute_sends_every_plan_of_an_unchanged_partition() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans = three_plans();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect("first run");

        let embedder = counting(8);
        let second = embed_chunks(
            &plans,
            EmbedSource::Recompute,
            &embedder,
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect("second run");
        assert_eq!(embedder.sent(), ["prose 1", "prose 2", "prose 3"]);
        assert_eq!(second.chunks_reused, 0);
        assert_eq!(partition_rows(dir.path()).await, 3);
    }

    #[tokio::test]
    async fn rows_on_an_unstamped_index_are_not_a_source() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ChunkStore::open_for_write(dir.path(), 8)
            .await
            .expect("create table");
        store
            .append(&[row_of(&plan(1, "prose"), vec![0.5f32; 8])])
            .await
            .expect("seed row");
        let corpus = Corpus::open_in_memory().expect("corpus");
        let embedder = counting(8);
        let report = embed_chunks(
            &[plan(1, "prose")],
            EmbedSource::SamePartition,
            &embedder,
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(embedder.sent(), ["prose"], "no stamp names the rows' model");
        assert_eq!(report.chunks_reused, 0);
        assert_eq!(partition_rows(dir.path()).await, 1);
    }

    #[tokio::test]
    async fn a_run_that_writes_nothing_reports_its_total_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans = three_plans();
        let corpus = Corpus::open_in_memory().expect("corpus");
        // One chunk to a batch: a run that embedded would report thrice.
        let cfg = EmbedConfig {
            batch_max_chunks: 1,
            ..EmbedConfig::default()
        };
        let source = EmbedSource::SamePartition;
        embed_chunks(&plans, source, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("first run");

        let seen = std::sync::Mutex::new(Vec::new());
        embed_chunks_with_progress(
            &plans,
            source,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
            &|consumed, total| seen.lock().unwrap().push((consumed, total)),
        )
        .await
        .expect("second run");
        assert_eq!(*seen.lock().unwrap(), vec![(3, 3)]);
    }

    #[tokio::test]
    async fn a_partly_reused_run_reports_rising_counts_ending_at_the_total() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut plans: Vec<ChunkPlan> = (1..=5).map(|i| plan(i, &format!("prose {i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig {
            batch_max_chunks: 2,
            ..EmbedConfig::default()
        };
        let source = EmbedSource::SamePartition;
        embed_chunks(&plans, source, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("first run");

        plans[0] = plan(1, "prose rewritten");
        plans[4] = plan(5, "prose rewritten too");
        let seen = std::sync::Mutex::new(Vec::new());
        embed_chunks_with_progress(
            &plans,
            source,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
            &|consumed, total| seen.lock().unwrap().push((consumed, total)),
        )
        .await
        .expect("second run");
        // Three reused rows land in slices of two, then the one batch
        // of two embedded plans.
        assert_eq!(*seen.lock().unwrap(), vec![(2, 5), (3, 5), (5, 5)]);
    }

    /// A [`Scripted`] fake whose vector for a text is `dim` copies of the
    /// text's length, so two texts of different lengths get different
    /// vectors.
    fn length_valued(dim: usize) -> Scripted {
        Scripted::new(move |_, texts: &[String]| {
            Ok(texts
                .iter()
                .map(|text| vec![text.chars().count() as f32; dim])
                .collect())
        })
    }

    #[tokio::test]
    async fn a_text_repeated_within_an_item_is_sent_once_and_stored_for_each_chunk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans = vec![
            plan(1, "a repeated line"),
            plan(2, "prose"),
            plan(3, "a repeated line"),
        ];
        let corpus = Corpus::open_in_memory().expect("corpus");
        let embedder = length_valued(8);
        let report = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &embedder,
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(embedder.sent(), ["a repeated line", "prose"]);
        assert_eq!(
            report,
            EmbedRunReport {
                chunks_written: 3,
                chunks_reused: 0,
                batches: 1,
                shrink_events: 0,
                total_chars: "a repeated line".len() + "prose".len(),
            }
        );
        let stored: Vec<(String, Vec<f32>)> = stored_rows(dir.path())
            .await
            .into_iter()
            .map(|row| (row.text, row.vector))
            .collect();
        assert_eq!(
            stored,
            [
                ("a repeated line".to_string(), vec![15.0; 8]),
                ("prose".to_string(), vec![5.0; 8]),
                ("a repeated line".to_string(), vec![15.0; 8]),
            ]
        );
    }

    #[tokio::test]
    async fn a_repeated_text_lands_with_the_batch_that_embedded_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans = vec![
            plan(1, "a repeated line"),
            plan(2, "prose"),
            plan(3, "a repeated line"),
            plan(4, "more prose"),
        ];
        let corpus = Corpus::open_in_memory().expect("corpus");
        // One text to a batch: the repeat rides with the first batch.
        let cfg = EmbedConfig {
            batch_max_chunks: 1,
            ..EmbedConfig::default()
        };
        let seen = std::sync::Mutex::new(Vec::new());
        let report = embed_chunks_with_progress(
            &plans,
            EmbedSource::SamePartition,
            &length_valued(8),
            &corpus,
            dir.path(),
            &cfg,
            &|consumed, total| seen.lock().unwrap().push((consumed, total)),
        )
        .await
        .expect("embed");
        assert_eq!(report.batches, 3);
        assert_eq!(*seen.lock().unwrap(), vec![(2, 4), (3, 4), (4, 4)]);
        assert_eq!(partition_rows(dir.path()).await, 4);
    }

    #[tokio::test]
    async fn a_repeated_text_among_reused_rows_keeps_the_count_rising() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut plans: Vec<ChunkPlan> = (1..=4).map(|i| plan(i, &format!("prose {i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        let source = EmbedSource::SamePartition;
        embed_chunks(&plans, source, &Fake { dim: 8 }, &corpus, dir.path(), &cfg)
            .await
            .expect("first run");

        plans[1] = plan(2, "a repeated line");
        plans[3] = plan(4, "a repeated line");
        let embedder = counting(8);
        let seen = std::sync::Mutex::new(Vec::new());
        let report = embed_chunks_with_progress(
            &plans,
            source,
            &embedder,
            &corpus,
            dir.path(),
            &cfg,
            &|consumed, total| seen.lock().unwrap().push((consumed, total)),
        )
        .await
        .expect("second run");
        assert_eq!(embedder.sent(), ["a repeated line"]);
        assert_eq!(report.chunks_written, 4);
        assert_eq!(report.chunks_reused, 2, "a repeat is not a reused row");
        // The two reused rows, then the one embedded text and its repeat.
        assert_eq!(*seen.lock().unwrap(), vec![(2, 4), (4, 4)]);
    }

    fn identity(embed_model: &str, vector_dim: u32) -> VectorIdentity<'_> {
        VectorIdentity {
            embed_model,
            vector_dim,
        }
    }

    /// One stored row per plan, each carrying a distinct usable vector
    /// of width 4: the row at position `i` holds `i + 1` in every slot.
    fn rows_for(plans: &[ChunkPlan]) -> Vec<ChunkRow> {
        plans
            .iter()
            .enumerate()
            .map(|(i, plan)| row_of(plan, vec![i as f32 + 1.0; 4]))
            .collect()
    }

    #[test]
    fn each_plan_takes_the_vector_stored_under_its_text() {
        let plans = three_plans();
        let rows = rows_for(&plans);
        let id = identity("model-a", 4);
        assert_eq!(
            reuse_vectors(Some((&id, &rows)), &id, &plans),
            [Some(vec![1.0; 4]), Some(vec![2.0; 4]), Some(vec![3.0; 4])]
        );
    }

    #[test]
    fn nothing_is_reused_without_a_source_or_across_identities() {
        let plans = three_plans();
        let rows = rows_for(&plans);
        let target = identity("model-a", 4);
        assert_eq!(reuse_vectors(None, &target, &plans), [None, None, None]);
        let other_model = identity("model-b", 4);
        assert_eq!(
            reuse_vectors(Some((&other_model, &rows)), &target, &plans),
            [None, None, None]
        );
        let other_width = identity("model-a", 8);
        assert_eq!(
            reuse_vectors(Some((&other_width, &rows)), &target, &plans),
            [None, None, None]
        );
    }

    #[test]
    fn a_stored_vector_that_cannot_be_used_is_passed_over() {
        let plans = three_plans();
        let id = identity("model-a", 4);
        for bad in [vec![1.0; 3], vec![0.0; 4], vec![f32::NAN; 4]] {
            let mut rows = rows_for(&plans);
            rows[1].vector = bad.clone();
            assert_eq!(
                reuse_vectors(Some((&id, &rows)), &id, &plans),
                [Some(vec![1.0; 4]), None, Some(vec![3.0; 4])],
                "{bad:?}"
            );
        }
    }

    #[test]
    fn of_several_rows_with_one_text_the_first_by_coordinates_is_taken() {
        let plans = vec![plan(1, "same"), plan(2, "same")];
        let mut rows = rows_for(&plans);
        // The row at the later coordinates arrives first.
        rows.reverse();
        let id = identity("model-a", 4);
        assert_eq!(
            reuse_vectors(Some((&id, &rows)), &id, &plans),
            [Some(vec![1.0; 4]), Some(vec![1.0; 4])]
        );
    }

    #[test]
    fn rows_in_another_order_are_still_unchanged() {
        let plans = three_plans();
        let mut rows = rows_for(&plans);
        rows.swap(0, 2);
        assert!(rows_unchanged(&rows, &plans, 4));
    }

    #[test]
    fn rows_are_changed_when_their_count_or_content_differs() {
        let plans = three_plans();
        let rows = rows_for(&plans);
        assert!(!rows_unchanged(&rows[..2], &plans, 4), "a row is missing");
        let mut extra = rows.clone();
        extra.push(rows[0].clone());
        assert!(!rows_unchanged(&extra, &plans, 4), "a row is doubled");
        let mut moved = rows.clone();
        moved[1].end_char_offset += 1;
        assert!(!rows_unchanged(&moved, &plans, 4), "a coordinate moved");
        let mut rehashed = rows.clone();
        rehashed[1].norm_chunk_sha256 = "0".repeat(64);
        assert!(!rows_unchanged(&rehashed, &plans, 4), "a hash differs");
    }

    #[test]
    fn two_texts_trading_places_under_the_same_hashes_are_a_change() {
        let plans = three_plans();
        let mut rows = rows_for(&plans);
        let (first, second) = (rows[0].text.clone(), rows[1].text.clone());
        rows[0].text = second;
        rows[1].text = first;
        assert!(!rows_unchanged(&rows, &plans, 4));
    }

    #[test]
    fn a_stored_vector_that_cannot_be_used_makes_the_rows_changed() {
        let plans = three_plans();
        for bad in [vec![1.0; 3], vec![0.0; 4], vec![f32::NAN; 4]] {
            let mut rows = rows_for(&plans);
            rows[1].vector = bad.clone();
            assert!(!rows_unchanged(&rows, &plans, 4), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn embeds_every_chunk_and_writes_one_row_each() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=5)
            .map(|i| plan(i, &format!("some prose text {i}")))
            .collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let report = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(report.chunks_written, 5);
        // One greedy batch holds all five short chunks, with no shrinking.
        assert_eq!(report.batches, 1);
        assert_eq!(report.shrink_events, 0);

        let store = ChunkStore::open_for_write(dir.path(), 8)
            .await
            .expect("reopen");
        assert_eq!(store.count_rows().await.expect("count"), 5);
    }

    #[tokio::test]
    async fn each_written_batch_is_reported_ending_at_the_plan_count() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=5)
            .map(|i| plan(i, &format!("some prose text {i}")))
            .collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig {
            batch_max_chunks: 2,
            ..EmbedConfig::default()
        };
        let calls = std::sync::Mutex::new(Vec::new());
        let report = embed_chunks_with_progress(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
            &|consumed, total| calls.lock().unwrap().push((consumed, total)),
        )
        .await
        .expect("embed");
        assert_eq!(report.batches, 3);
        assert_eq!(*calls.lock().unwrap(), vec![(2, 5), (4, 5), (5, 5)]);
    }

    #[tokio::test]
    async fn an_empty_plan_list_reports_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let corpus = Corpus::open_in_memory().expect("corpus");
        let calls = std::sync::Mutex::new(Vec::new());
        embed_chunks_with_progress(
            &[],
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
            &|consumed, total| calls.lock().unwrap().push((consumed, total)),
        )
        .await
        .expect("embed");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn scale_to_corpus_clamps_partitions_for_small_libraries() {
        let cfg = bookrack_vectors::AnnConfig::default_for(AnnKind::IvfFlat);
        // Default num_partitions=64; 5 chunks → max 1 partition.
        let scaled = scale_to_corpus(cfg.clone(), 5);
        assert_eq!(scaled.num_partitions, 1);
        // 1024 chunks → max 4 partitions.
        let scaled = scale_to_corpus(cfg.clone(), 1024);
        assert_eq!(scaled.num_partitions, 4);
        // 66_703 chunks → keep the requested 64.
        let scaled = scale_to_corpus(cfg, 66_703);
        assert_eq!(scaled.num_partitions, 64);
    }

    #[test]
    fn scale_to_corpus_leaves_brute_force_alone() {
        let cfg = bookrack_vectors::AnnConfig::default_for(AnnKind::BruteForce);
        assert_eq!(scale_to_corpus(cfg, 0).num_partitions, 0);
    }

    #[tokio::test]
    async fn cold_start_writes_a_scaled_ivf_flat_meta_after_first_book() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=5)
            .map(|i| plan(i, &format!("some text {i}")))
            .collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        assert_eq!(meta.kind, "ivf-flat");
        // 5 chunks is well under sample_rate × num_partitions for the
        // default num_partitions=64, so the cold start clamps to 1.
        assert_eq!(meta.num_partitions, 1);
        assert_eq!(meta.default_nprobes, 40);
        assert_eq!(meta.built_at_chunk_count, 5);
        assert_eq!(meta.churn_since_rebuild, 0);
    }

    #[tokio::test]
    async fn brute_force_meta_is_respected_and_not_auto_rebuilt() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Seed an explicit brute-force meta: a previous `vectors drop`.
        let seed = bookrack_vectors::AnnConfig::default_for(AnnKind::BruteForce).to_meta(
            "2026-06-03T00:00:00Z".to_string(),
            0,
            0,
            bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
        );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=5)
            .map(|i| plan(i, &format!("some text {i}")))
            .collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        assert_eq!(meta.kind, "brute-force");
        // Churn does grow, but no retrain happens.
        assert_eq!(meta.churn_since_rebuild, 5);
    }

    #[tokio::test]
    async fn churn_below_threshold_does_not_trigger_retrain() {
        let dir = tempfile::tempdir().expect("temp dir");
        let original_ts = "2026-06-03T00:00:00Z";
        // built_at_chunk_count=100 → threshold = max(200, 20000) = 20000.
        // Only 5 churn from one small ingest is well below.
        let seed = bookrack_vectors::AnnConfig::default_for(AnnKind::IvfFlat).to_meta(
            original_ts.to_string(),
            100,
            0,
            bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
        );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=5)
            .map(|i| plan(i, &format!("some text {i}")))
            .collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        assert_eq!(meta.built_at, original_ts);
        assert_eq!(meta.built_at_chunk_count, 100);
        assert_eq!(meta.churn_since_rebuild, 5);
    }

    #[tokio::test]
    async fn churn_above_threshold_triggers_retrain_and_resets_counter() {
        let dir = tempfile::tempdir().expect("temp dir");
        // built_at_chunk_count=2 → threshold = max(4, 20000) = 20000.
        // Pre-seed churn at 19_999; a 5-chunk ingest pushes it past.
        let seed = bookrack_vectors::AnnConfig::default_for(AnnKind::IvfFlat).to_meta(
            "2026-06-03T00:00:00Z".to_string(),
            2,
            19_999,
            bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
        );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=5)
            .map(|i| plan(i, &format!("some text {i}")))
            .collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        // Retrain happened: counter reset, built_at_chunk_count updated.
        assert_eq!(meta.churn_since_rebuild, 0);
        assert_eq!(meta.built_at_chunk_count, 5);
        // Partitions scaled down from the seeded 64 to fit corpus.
        assert_eq!(meta.num_partitions, 1);
    }

    #[tokio::test]
    async fn churn_grows_by_inserted_rows_on_first_book_when_meta_exists() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Seed a meta file claiming an index was already built at 0 rows.
        let seed = bookrack_vectors::AnnConfig::default_for(bookrack_vectors::AnnKind::IvfFlat)
            .to_meta(
                "2026-06-03T00:00:00Z".to_string(),
                0,
                0,
                bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
            );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=5)
            .map(|i| plan(i, &format!("some text {i}")))
            .collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        // First book: 0 deleted + 5 inserted = 5 churn.
        assert_eq!(meta.churn_since_rebuild, 5);
    }

    #[tokio::test]
    async fn re_embedding_the_same_book_accumulates_delete_and_append_churn() {
        let dir = tempfile::tempdir().expect("temp dir");
        let seed = bookrack_vectors::AnnConfig::default_for(bookrack_vectors::AnnKind::IvfFlat)
            .to_meta(
                "2026-06-03T00:00:00Z".to_string(),
                0,
                0,
                bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
            );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, &format!("text {i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig::default();
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect("first embed");
        embed_chunks(
            &plans,
            EmbedSource::Recompute,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect("second embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        // First run: 0 + 3 = 3. The second recomputes every vector:
        // 3 deleted + 3 inserted = 6. Cumulative: 3 + 6 = 9.
        assert_eq!(meta.churn_since_rebuild, 9);
    }

    #[tokio::test]
    async fn a_refused_write_leaves_vectors_meta_as_it_was() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, &format!("prose {i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let first = EmbedConfig {
            model: "model-a".to_string(),
            ..EmbedConfig::default()
        };
        // First run: cold-start writes a meta.
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &first,
        )
        .await
        .expect("first run");
        assert!(
            bookrack_vectors::meta::load(dir.path())
                .expect("load")
                .is_some(),
            "first run should leave a meta",
        );
        let before = std::fs::read(dir.path().join("vectors_meta.json")).expect("meta bytes");
        // Second run with a different model is refused before anything
        // is written: the chunks table stands, and so do the index
        // declaration and churn count that describe it.
        let second = EmbedConfig {
            model: "model-b".to_string(),
            ..EmbedConfig::default()
        };
        let embedder = counting(8);
        let err = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &embedder,
            &corpus,
            dir.path(),
            &second,
        )
        .await
        .expect_err("model swap must be refused");
        assert_eq!(embedder.calls(), 0, "the refusal precedes any request");
        assert!(matches!(
            err,
            EmbedStageError::Corpus(bookrack_corpus::CorpusError::IndexStampMismatch { .. })
        ));
        let after = std::fs::read(dir.path().join("vectors_meta.json"))
            .expect("a refused write must leave vectors_meta.json in place");
        assert_eq!(after, before, "a refused write must not rewrite the file");
    }

    #[tokio::test]
    async fn empty_plans_write_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let corpus = Corpus::open_in_memory().expect("corpus");
        let report = embed_chunks(
            &[],
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(report, EmbedRunReport::default());
    }

    #[tokio::test]
    async fn re_embedding_replaces_rather_than_duplicates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, &format!("text {i}"))).collect();
        let cfg = EmbedConfig::default();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect("first");
        embed_chunks(
            &plans,
            EmbedSource::Recompute,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect("second");

        let store = ChunkStore::open_for_write(dir.path(), 8)
            .await
            .expect("reopen");
        // The second run deletes the partition before appending, so the
        // count stays at three rather than doubling to six.
        assert_eq!(store.count_rows().await.expect("count"), 3);
    }

    #[tokio::test]
    async fn an_overloaded_batch_shrinks_and_still_writes_every_chunk() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Many small chunks in one greedy batch; the embedder refuses any
        // batch above two, forcing repeated halving down to that size.
        let plans: Vec<ChunkPlan> = (1..=8).map(|i| plan(i, &letter(i))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let report = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Overloading {
                dim: 4,
                max_batch: 2,
            },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(report.chunks_written, 8);
        // The eight-chunk batch is halved until each piece is within the
        // server's limit, so the run records the shrinking it took.
        assert!(report.shrink_events > 0);
    }

    #[tokio::test]
    async fn a_run_stamps_the_index_and_then_refuses_a_different_model() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, &format!("prose {i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let first = EmbedConfig {
            model: "model-a".to_string(),
            ..EmbedConfig::default()
        };
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &first,
        )
        .await
        .expect("first run stamps the index");
        assert_eq!(
            corpus
                .meta_get(bookrack_corpus::EMBED_MODEL_KEY)
                .expect("get"),
            Some("model-a".to_string())
        );

        // A second book embedded with a different model is refused before
        // any vector is written, so the index is never mixed.
        let second = EmbedConfig {
            model: "model-b".to_string(),
            ..EmbedConfig::default()
        };
        let err = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &second,
        )
        .await
        .expect_err("a different model must be refused");
        assert!(matches!(
            err,
            EmbedStageError::Corpus(CorpusError::IndexStampMismatch { key, .. })
                if key == bookrack_corpus::EMBED_MODEL_KEY
        ));
    }

    #[test]
    fn progress_ticker_emits_the_first_tick_unconditionally() {
        let mut ticker = ProgressTicker::new(Duration::from_secs(60));
        // The first call always emits — users get a sign of life on the
        // first batch even if the interval is long.
        assert!(ticker.should_emit(Instant::now()));
    }

    #[test]
    fn progress_ticker_throttles_subsequent_ticks_to_the_interval() {
        let base = Instant::now();
        let mut ticker = ProgressTicker::new(Duration::from_secs(5));
        assert!(ticker.should_emit(base));
        // Two seconds later: under the 5 s interval, no tick.
        assert!(!ticker.should_emit(base + Duration::from_secs(2)));
        // Five seconds after the previous emission: the interval has
        // elapsed, tick again.
        assert!(ticker.should_emit(base + Duration::from_secs(5)));
        // Immediately after: throttled.
        assert!(!ticker.should_emit(base + Duration::from_secs(5)));
    }

    #[test]
    fn progress_ticker_with_a_zero_interval_emits_on_every_call() {
        let mut ticker = ProgressTicker::new(Duration::ZERO);
        let now = Instant::now();
        assert!(ticker.should_emit(now));
        assert!(ticker.should_emit(now));
        assert!(ticker.should_emit(now));
    }

    /// A fake whose vectors are empty.
    struct Hollow;

    impl Embedder for Hollow {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let n = texts.len();
            async move { Ok(vec![Vec::new(); n]) }
        }
    }

    #[tokio::test]
    async fn a_first_batch_answered_with_an_empty_vector_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let lancedb_dir = dir.path().join("lancedb");
        let corpus = Corpus::open_in_memory().expect("corpus");
        let err = embed_chunks(
            &[plan(1, "prose")],
            EmbedSource::SamePartition,
            &Hollow,
            &corpus,
            &lancedb_dir,
            &EmbedConfig::default(),
        )
        .await
        .expect_err("a zero-width vector fixes no dimension");
        assert!(matches!(err, EmbedStageError::EmptyEmbedding), "{err:?}");
        assert!(
            ChunkReader::open(&lancedb_dir)
                .await
                .expect("open reader")
                .is_none(),
            "a refused run must not create the table",
        );
    }

    #[tokio::test]
    async fn the_report_counts_batches_and_the_characters_embedded() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Three distinct texts of five characters each.
        let plans: Vec<ChunkPlan> = (1..=3).map(|i| plan(i, &format!("pros{i}"))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let cfg = EmbedConfig {
            batch_max_chunks: 1,
            ..EmbedConfig::default()
        };
        let report = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &cfg,
        )
        .await
        .expect("embed");
        assert_eq!(
            report,
            EmbedRunReport {
                chunks_written: 3,
                chunks_reused: 0,
                batches: 3,
                shrink_events: 0,
                total_chars: 15,
            }
        );
    }

    #[tokio::test]
    async fn churn_exactly_at_the_threshold_triggers_retrain() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Threshold = max(2 * 2, 20_000) = 20_000; five rows on top of
        // 19_995 land exactly on it.
        let seed = bookrack_vectors::AnnConfig::default_for(AnnKind::IvfFlat).to_meta(
            "2026-06-03T00:00:00Z".to_string(),
            2,
            L2_ABSOLUTE_FLOOR - 5,
            bookrack_vectors::DEFAULT_INDEX_NAME.to_string(),
        );
        bookrack_vectors::meta::store(dir.path(), &seed).expect("seed meta");

        let plans: Vec<ChunkPlan> = (1..=5)
            .map(|i| plan(i, &format!("some text {i}")))
            .collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Fake { dim: 8 },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        let meta = bookrack_vectors::meta::load(dir.path())
            .expect("load meta")
            .expect("meta present");
        assert_eq!(meta.churn_since_rebuild, 0, "the index was retrained");
        assert_eq!(meta.built_at_chunk_count, 5);
    }

    #[test]
    fn now_rfc3339_is_a_utc_timestamp_at_second_precision() {
        let now = now_rfc3339();
        assert_eq!(now.len(), "2024-06-05T07:40:00Z".len(), "{now}");
        assert!(now.ends_with('Z'), "{now}");
        chrono::DateTime::parse_from_rfc3339(&now).expect("an RFC 3339 timestamp");
    }

    /// Plans whose texts have the given character counts.
    fn plans_of(lens: &[usize]) -> Vec<ChunkPlan> {
        lens.iter()
            .enumerate()
            .map(|(i, len)| plan(i as i64 + 1, &"x".repeat(*len)))
            .collect()
    }

    fn refs(plans: &[ChunkPlan]) -> Vec<&ChunkPlan> {
        plans.iter().collect()
    }

    fn batching(batch_char_budget: usize, batch_max_chunks: usize) -> EmbedConfig {
        EmbedConfig {
            batch_char_budget,
            batch_max_chunks,
            ..EmbedConfig::default()
        }
    }

    #[test]
    fn a_batch_fills_to_the_character_budget_and_not_past_it() {
        // Three chunks of two fill a budget of six exactly.
        assert_eq!(
            greedy_batch_end(&refs(&plans_of(&[2, 2, 2])), 0, &batching(6, 99)),
            3
        );
        // Three and four overrun it by one: the second chunk waits.
        assert_eq!(
            greedy_batch_end(&refs(&plans_of(&[3, 4])), 0, &batching(6, 99)),
            1
        );
        // The budget counts from the batch's own start.
        assert_eq!(
            greedy_batch_end(&refs(&plans_of(&[5, 3, 3, 3])), 1, &batching(6, 99)),
            3
        );
    }

    #[test]
    fn a_chunk_over_the_budget_is_a_batch_of_its_own() {
        assert_eq!(
            greedy_batch_end(&refs(&plans_of(&[10, 1])), 0, &batching(5, 99)),
            1
        );
        assert_eq!(
            greedy_batch_end(&refs(&plans_of(&[1, 10, 1])), 1, &batching(5, 99)),
            2
        );
    }

    #[test]
    fn a_batch_stops_at_the_chunk_cap() {
        assert_eq!(
            greedy_batch_end(&refs(&plans_of(&[1, 1, 1, 1])), 0, &batching(99, 2)),
            2
        );
        assert_eq!(
            greedy_batch_end(&refs(&plans_of(&[1, 1, 1, 1])), 2, &batching(99, 3)),
            4
        );
    }

    #[tokio::test]
    async fn every_halving_is_counted_as_one_shrink_event() {
        let dir = tempfile::tempdir().expect("temp dir");
        // One batch of four against a server that takes one chunk at a
        // time: four halves to two and two, and each two halves again.
        let plans: Vec<ChunkPlan> = (1..=4).map(|i| plan(i, &letter(i))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let report = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &Overloading {
                dim: 4,
                max_batch: 1,
            },
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect("embed");
        assert_eq!(report.chunks_written, 4);
        assert_eq!(report.batches, 1);
        assert_eq!(report.shrink_events, 3);
    }

    /// A fake that fails every request, counting the requests it
    /// receives.
    struct Failing {
        overloaded: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Failing {
        fn new(overloaded: bool) -> Self {
            Failing {
                overloaded,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Embedder for Failing {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = texts;
            let overloaded = self.overloaded;
            async move {
                if overloaded {
                    Err(EmbedError::Overloaded {
                        status: 503,
                        body: String::new(),
                    })
                } else {
                    Err(EmbedError::Unreachable("refused".to_string()))
                }
            }
        }
    }

    #[tokio::test]
    async fn an_overloaded_single_chunk_fails_without_another_request() {
        let dir = tempfile::tempdir().expect("temp dir");
        let corpus = Corpus::open_in_memory().expect("corpus");
        let embedder = Failing::new(true);
        let err = embed_chunks(
            &[plan(1, "prose")],
            EmbedSource::SamePartition,
            &embedder,
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect_err("one chunk cannot be halved");
        assert!(
            matches!(err, EmbedStageError::Embed(EmbedError::Overloaded { .. })),
            "{err:?}"
        );
        assert_eq!(embedder.calls(), 1, "the one batch, sent once");
    }

    #[tokio::test]
    async fn a_failure_that_is_not_overload_does_not_shrink_the_batch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plans: Vec<ChunkPlan> = (1..=4).map(|i| plan(i, &letter(i))).collect();
        let corpus = Corpus::open_in_memory().expect("corpus");
        let embedder = Failing::new(false);
        let err = embed_chunks(
            &plans,
            EmbedSource::SamePartition,
            &embedder,
            &corpus,
            dir.path(),
            &EmbedConfig::default(),
        )
        .await
        .expect_err("an unreachable server fails the run");
        assert!(
            matches!(err, EmbedStageError::Embed(EmbedError::Unreachable(_))),
            "{err:?}"
        );
        assert_eq!(embedder.calls(), 1, "the one batch, sent once");
    }
}
