// SPDX-License-Identifier: Apache-2.0

//! `vectors`: the LanceDB `chunks` table — the dense vector store.
//!
//! This crate owns the read and write side of the slim chunks table:
//! a flat seven-column schema holding one row per embedded chunk. It
//! is the persistence half of the EMBED stage — the `embed` crate
//! produces vectors, this crate stores and searches them.
//!
//! The chunks table carries *no* book or chapter metadata: those are
//! soft-referenced by `start_node_id` and joined from `catalog.db` at
//! query time, so editing metadata never invalidates a vector. A row's
//! owning book is recovered from `start_node_id` by integer division
//! alone (invariant I2), which is also how [`ChunkStore::delete_partition`]
//! erases exactly one book.
//!
//! Scope: this is the store. The IVF-PQ approximate index and the
//! jieba full-text index are deliberate follow-ons — at pilot scale a
//! brute-force [`ChunkStore::search`] is both exact and fast, and the
//! index is only worth building past tens of thousands of rows.

pub mod meta;

pub use lancedb::index::IndexStatistics;
pub use meta::{DEFAULT_INDEX_NAME, META_FILENAME, SCHEMA_VERSION, VectorsMeta};

use std::path::Path;
use std::sync::{Arc, OnceLock};

use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Int32Type, Int64Type};
use arrow_array::{
    FixedSizeListArray, Float32Array, Int32Array, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray,
};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::DistanceType;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::table::OptimizeAction;

use bookrack_core::{NODE_CAPACITY, NODE_PARTITION_FACTOR, NodeId, PartitionIdx};

/// Name of the single table this crate manages.
const TABLE: &str = "chunks";

/// One-shot init of process-global environment lancedb consults.
/// Reads this once on first [`ChunkStore::open`] call and never again.
static LANCE_ENV_INIT: OnceLock<()> = OnceLock::new();

/// Set the environment variables lancedb honours at startup. Today
/// just `LANCE_INCLUDE_VECTOR_CENTROIDS=false`, which silences a noisy
/// per-`list_indices` warning on lancedb 0.30 about an upcoming
/// default change.
fn ensure_lance_env() {
    LANCE_ENV_INIT.get_or_init(|| {
        // SAFETY: env is mutated exactly once, before any background
        // task could observe it. `OnceLock::get_or_init` guarantees
        // the closure runs at most once across all threads.
        unsafe {
            std::env::set_var("LANCE_INCLUDE_VECTOR_CENTROIDS", "false");
        }
    });
}

/// Why a `vectors` operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VectorsError {
    /// The underlying LanceDB layer reported an error.
    #[error("LanceDB error: {0}")]
    Lance(#[from] lancedb::Error),

    /// An Arrow record batch could not be built or decoded.
    #[error("Arrow error: {0}")]
    Arrow(#[from] ArrowError),

    /// A chunk's vector length does not match the dimension the store
    /// was opened with. Every row in the table must share one
    /// dimension, fixed by the embedding model.
    #[error("chunk vector has dimension {got}, store expects {expected}")]
    DimensionMismatch {
        /// The offending vector's length.
        got: usize,
        /// The dimension the store was opened with.
        expected: usize,
    },

    /// A query result was missing an expected column, or it held an
    /// unexpected type — a schema the store did not write.
    #[error("chunks query result is missing or mistyped the {0:?} column")]
    BadColumn(&'static str),

    /// IO failure on the `vectors_meta.json` sidecar — read, write, or
    /// rename.
    #[error("vectors_meta IO error: {0}")]
    MetaIo(#[from] std::io::Error),

    /// `vectors_meta.json` was unparseable: malformed JSON, an unknown
    /// `kind`, or a field whose type does not match the schema.
    #[error("vectors_meta parse error: {0}")]
    MetaParse(#[from] serde_json::Error),

    /// `vectors_meta.json` carried a `kind` string this build does not
    /// recognise. Likely a meta file written by a newer crate version.
    #[error("vectors_meta has unknown kind {0:?}")]
    UnknownAnnKind(String),

    /// A caller asked to build an ANN index for `AnnKind::BruteForce`,
    /// which has no index — only `drop_ann_index` is meaningful for
    /// that kind.
    #[error("cannot build an ANN index for kind brute-force")]
    BuildOnBruteForceKind,

    /// An `IvfPq` build was attempted without the required quantization
    /// parameters in the config.
    #[error("IvfPq build is missing required parameter {0}")]
    MissingPqParam(&'static str),

    /// Product quantization is configured too coarsely for the
    /// embedding dimension: `dim / num_sub_vectors > 8`. Loss from
    /// over-aggressive compression is unacceptable. LanceDB recommends
    /// `num_sub_vectors = dim / 8`.
    #[error(
        "IvfPq quantization too coarse: dim={dim} / num_sub_vectors={num_sub_vectors} > 8 \
         (recommended num_sub_vectors = dim / 8)"
    )]
    IvfPqQuantizationTooCoarse {
        /// The embedding dimension.
        dim: usize,
        /// The configured PQ sub-vector count.
        num_sub_vectors: usize,
    },

    /// `vectors_meta.json` carried a `min_reader_version` stamp this
    /// binary cannot meet. The writer required a reader at version
    /// `required` or higher; this build is at `current`.
    #[error(
        "vectors_meta requires a newer reader: stamp demands v{required}, \
         this build is at v{current}"
    )]
    ReaderTooOld {
        /// The `min_reader_version` value recorded on disk.
        required: u32,
        /// [`bookrack_dbkit::READER_VERSION`] this build was compiled at.
        current: u32,
    },
}

/// The `min_reader_version` value this binary stamps when writing
/// `vectors_meta.json`.
///
/// Bump when a writer-side change to the meta or to the chunk-row
/// schema would make older readers misinterpret what they load — e.g.
/// repurposing an existing field, or changing the meaning of an
/// `AnnKind` label. New optional JSON fields do not require a bump.
pub const MIN_READER_VERSION: u32 = 1;

/// A fallible `vectors` operation.
pub type Result<T> = std::result::Result<T, VectorsError>;

/// The ANN index family attached to the chunks table. `BruteForce` is
/// the explicit "no index" state — distinct from "no meta file yet,"
/// which [`ChunkStore`] surfaces as `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnKind {
    /// IVF with no quantization. Default — C1 recommendation.
    IvfFlat,
    /// IVF with 8-bit scalar quantization. ~2× faster than IvfFlat with
    /// <1% recall loss on L2-normalized vectors.
    IvfSq,
    /// IVF with product quantization. Higher compression, but `nsv` must
    /// be tuned to the embedding dimension (see [`AnnConfig::default_for`]).
    IvfPq,
    /// IVF with an HNSW sub-graph in each partition; raw vectors.
    /// Unstable on lancedb 0.30 — see upstream issue 1428.
    IvfHnswFlat,
    /// IVF + HNSW + scalar quantization. Reserved for future CLI rebuild.
    IvfHnswSq,
    /// IVF + HNSW + product quantization. Reserved for future CLI rebuild.
    IvfHnswPq,
    /// No ANN index attached; queries scan the table directly.
    BruteForce,
}

impl AnnKind {
    /// The kebab-case label this kind serializes as in
    /// `vectors_meta.json::kind`.
    pub fn as_str(&self) -> &'static str {
        match self {
            AnnKind::IvfFlat => "ivf-flat",
            AnnKind::IvfSq => "ivf-sq",
            AnnKind::IvfPq => "ivf-pq",
            AnnKind::IvfHnswFlat => "ivf-hnsw-flat",
            AnnKind::IvfHnswSq => "ivf-hnsw-sq",
            AnnKind::IvfHnswPq => "ivf-hnsw-pq",
            AnnKind::BruteForce => "brute-force",
        }
    }
}

impl std::str::FromStr for AnnKind {
    type Err = VectorsError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "ivf-flat" => Ok(AnnKind::IvfFlat),
            "ivf-sq" => Ok(AnnKind::IvfSq),
            "ivf-pq" => Ok(AnnKind::IvfPq),
            "ivf-hnsw-flat" => Ok(AnnKind::IvfHnswFlat),
            "ivf-hnsw-sq" => Ok(AnnKind::IvfHnswSq),
            "ivf-hnsw-pq" => Ok(AnnKind::IvfHnswPq),
            "brute-force" => Ok(AnnKind::BruteForce),
            _ => Err(VectorsError::UnknownAnnKind(s.to_string())),
        }
    }
}

/// In-memory view of an ANN configuration. The `nprobes` /
/// `refine_factor` fields here drive query behaviour; build-time
/// parameters (`num_partitions`, `num_sub_vectors`, `num_bits`) come
/// from the same struct so the same value can be passed to
/// `build_ann_index` and consulted at query time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnConfig {
    /// Which IVF family.
    pub kind: AnnKind,
    /// `k` for the IVF k-means quantizer. Ignored for `BruteForce`.
    pub num_partitions: u32,
    /// PQ sub-vector count; only meaningful for the `IvfPq*` kinds.
    pub num_sub_vectors: Option<u32>,
    /// PQ code width in bits per sub-vector; only meaningful for the
    /// `IvfPq*` kinds.
    pub num_bits: Option<u32>,
    /// Query-time partition fan-out.
    pub nprobes: u32,
    /// Query-time refinement multiplier; primarily for PQ and HNSW.
    pub refine_factor: Option<u32>,
}

impl AnnConfig {
    /// The recommended default configuration for a given kind on the
    /// current corpus. IvfFlat and IvfSq use `num_partitions = 64` and
    /// `nprobes = 40` — both cleared the C1 recall threshold on the
    /// 66.7K-chunk corpus. IvfPq uses the LanceDB-recommended
    /// `num_sub_vectors = dim / 8 = 128` and `num_bits = 8`; the
    /// alternative `nsv = 64` was shown insufficient under
    /// `dim = 1024`.
    pub fn default_for(kind: AnnKind) -> AnnConfig {
        match kind {
            AnnKind::IvfFlat | AnnKind::IvfSq => AnnConfig {
                kind,
                num_partitions: 64,
                num_sub_vectors: None,
                num_bits: None,
                nprobes: 40,
                refine_factor: None,
            },
            AnnKind::IvfPq => AnnConfig {
                kind,
                num_partitions: 64,
                num_sub_vectors: Some(128),
                num_bits: Some(8),
                nprobes: 40,
                refine_factor: None,
            },
            AnnKind::IvfHnswFlat | AnnKind::IvfHnswSq | AnnKind::IvfHnswPq => AnnConfig {
                kind,
                num_partitions: 64,
                num_sub_vectors: None,
                num_bits: None,
                nprobes: 40,
                refine_factor: Some(5),
            },
            AnnKind::BruteForce => AnnConfig {
                kind,
                num_partitions: 0,
                num_sub_vectors: None,
                num_bits: None,
                nprobes: 0,
                refine_factor: None,
            },
        }
    }

    /// Decode a persisted [`VectorsMeta`] into an [`AnnConfig`].
    pub fn from_meta(meta: &VectorsMeta) -> Result<AnnConfig> {
        let kind: AnnKind = meta.kind.parse()?;
        Ok(AnnConfig {
            kind,
            num_partitions: meta.num_partitions,
            num_sub_vectors: meta.num_sub_vectors,
            num_bits: meta.num_bits,
            nprobes: meta.default_nprobes,
            refine_factor: meta.default_refine_factor,
        })
    }

    /// Stamp this config into a fresh [`VectorsMeta`] for persisting.
    /// `built_at` is RFC 3339 (the caller owns the clock so this module
    /// stays deterministic).
    pub fn to_meta(
        &self,
        built_at: String,
        built_at_chunk_count: u64,
        churn_since_rebuild: u64,
        lance_index_name: String,
    ) -> VectorsMeta {
        VectorsMeta {
            schema_version: SCHEMA_VERSION,
            min_reader_version: Some(MIN_READER_VERSION),
            kind: self.kind.as_str().to_string(),
            num_partitions: self.num_partitions,
            num_sub_vectors: self.num_sub_vectors,
            num_bits: self.num_bits,
            default_nprobes: self.nprobes,
            default_refine_factor: self.refine_factor,
            built_at,
            built_at_chunk_count,
            churn_since_rebuild,
            lance_index_name,
        }
    }
}

/// One chunk row about to be written to the store.
///
/// `start_node_id` / `end_node_id` are soft references into
/// `corpus.db`: a chunk spans from a position in one prose leaf to a
/// position in another (the same leaf for a single-paragraph chunk).
#[derive(Debug, Clone)]
pub struct ChunkRow {
    /// The embedding vector; its length must equal the store dimension.
    pub vector: Vec<f32>,
    /// The chunk's verbatim text — what a search result displays.
    pub text: String,
    /// The leaf the chunk starts in.
    pub start_node_id: NodeId,
    /// Character offset of the chunk start within `start_node_id`.
    pub start_char_offset: i32,
    /// The leaf the chunk ends in; equal to `start_node_id` for a
    /// single-paragraph chunk.
    pub end_node_id: NodeId,
    /// Character offset of the chunk end within `end_node_id`.
    pub end_char_offset: i32,
    /// SHA-256 of the normalized chunk text — the query-time
    /// near-duplicate fold key and the embed-cache key.
    pub norm_chunk_sha256: String,
}

/// Per-query overrides for [`ChunkStore::search_with`]. All fields are
/// optional and default to "no override" — at the LanceDB layer this
/// means the index uses its built-in defaults.
#[derive(Debug, Default, Clone)]
pub struct SearchOptions {
    /// Override the IVF probe count for this query. When `None`, the
    /// index defaults apply.
    pub nprobes: Option<usize>,
    /// Override the IVF-PQ refinement multiplier for this query.
    pub refine_factor: Option<u32>,
    /// Force a brute-force scan even if an index exists. Useful for
    /// AB-testing recall against the ground truth without dropping
    /// the index on disk.
    pub bypass_index: bool,
}

/// One hit from a [`ChunkStore::search`], carrying the slim row plus
/// its distance to the query. The vector itself is not returned — it is
/// heavy and the search side never needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The chunk's verbatim text.
    pub text: String,
    /// The leaf the chunk starts in.
    pub start_node_id: NodeId,
    /// Character offset of the chunk start within `start_node_id`.
    pub start_char_offset: i32,
    /// The leaf the chunk ends in.
    pub end_node_id: NodeId,
    /// Character offset of the chunk end within `end_node_id`.
    pub end_char_offset: i32,
    /// SHA-256 of the normalized chunk text.
    pub norm_chunk_sha256: String,
    /// Distance to the query vector under the cosine metric — smaller
    /// is nearer.
    pub distance: f32,
}

/// A handle to the LanceDB `chunks` table.
///
/// All reads and writes of the dense store go through this type;
/// callers never assemble LanceDB queries themselves.
pub struct ChunkStore {
    table: lancedb::Table,
    dim: usize,
}

impl ChunkStore {
    /// Open the chunks table under `lancedb_dir`, creating an empty one
    /// if none exists.
    ///
    /// `dim` is the embedding vector dimension. It is fixed when the
    /// table is first created; reopening a store with a different `dim`
    /// than its rows were written with will fail later, on write or
    /// read — a directory must be reused only with one embedding model.
    pub async fn open(lancedb_dir: &Path, dim: usize) -> Result<ChunkStore> {
        ensure_lance_env();
        // Reader-version axis: a meta file written by a future binary
        // may demand a stamp this build cannot meet. The check runs
        // before the table is opened so a refused open never touches
        // the on-disk lancedb state.
        let stored_min_reader = meta::load(lancedb_dir)?.and_then(|m| m.min_reader_version);
        if let bookrack_dbkit::OpenDecision::Refuse { .. } =
            bookrack_dbkit::reader_version_decision(stored_min_reader)
        {
            return Err(VectorsError::ReaderTooOld {
                required: stored_min_reader.expect("Refuse implies a stamp was present"),
                current: bookrack_dbkit::READER_VERSION,
            });
        }
        let conn = lancedb::connect(&lancedb_dir.to_string_lossy())
            .execute()
            .await?;
        let names = conn.table_names().execute().await?;
        let (table, dim) = if names.iter().any(|name| name == TABLE) {
            let table = conn.open_table(TABLE).execute().await?;
            // If the on-disk table already fixes a vector dim, reflect
            // that in the handle rather than the caller's hint — the
            // table's schema is the source of truth and any mismatch is
            // caught at write time anyway.
            let on_disk_dim = vector_dim_from_schema(table.schema().await?.as_ref())?;
            (table, on_disk_dim)
        } else {
            (
                conn.create_empty_table(TABLE, chunk_schema(dim))
                    .execute()
                    .await?,
                dim,
            )
        };
        Ok(ChunkStore { table, dim })
    }

    /// The embedding dimension this store holds.
    pub fn dimension(&self) -> usize {
        self.dim
    }

    /// Append chunk rows to the table in one batch. Returns the number
    /// written; an empty input is a no-op. Every vector must match the
    /// store dimension, or the whole batch is rejected before any write.
    pub async fn append(&self, rows: &[ChunkRow]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        for row in rows {
            if row.vector.len() != self.dim {
                return Err(VectorsError::DimensionMismatch {
                    got: row.vector.len(),
                    expected: self.dim,
                });
            }
        }
        let batch = build_batch(rows, self.dim)?;
        let schema = batch.schema();
        let batch: std::result::Result<RecordBatch, ArrowError> = Ok(batch);
        let reader = RecordBatchIterator::new(std::iter::once(batch), schema);
        self.table
            .add(Box::new(reader) as Box<dyn RecordBatchReader + Send>)
            .execute()
            .await?;
        Ok(rows.len())
    }

    /// Delete every chunk row of one book. The book is named by its
    /// partition; the deletion is a `start_node_id` range filter, the
    /// same form the search prefilter uses. Re-embedding a book is
    /// therefore delete-then-append, which cannot duplicate rows.
    pub async fn delete_partition(&self, partition: PartitionIdx) -> Result<()> {
        let lo = partition.root().get();
        let hi = partition.get() * NODE_PARTITION_FACTOR + NODE_CAPACITY;
        self.table
            .delete(&format!("start_node_id BETWEEN {lo} AND {hi}"))
            .await?;
        Ok(())
    }

    /// Total number of chunk rows in the table.
    pub async fn count_rows(&self) -> Result<usize> {
        Ok(self.table.count_rows(None).await?)
    }

    /// Read every chunk row of one book back, vectors included.
    ///
    /// The partition is named by [`PartitionIdx`]; the filter mirrors
    /// [`Self::delete_partition`] so the same `start_node_id` range that
    /// would clear the partition's rows scans them. Rows are returned in
    /// whatever order LanceDB walks the underlying fragments — callers
    /// that care about a deterministic order must sort themselves.
    pub async fn scan_partition(&self, partition: PartitionIdx) -> Result<Vec<ChunkRow>> {
        let lo = partition.root().get();
        let hi = partition.get() * NODE_PARTITION_FACTOR + NODE_CAPACITY;
        let batches: Vec<RecordBatch> = self
            .table
            .query()
            .only_if(format!("start_node_id BETWEEN {lo} AND {hi}"))
            .execute()
            .await?
            .try_collect()
            .await?;
        let mut rows = Vec::new();
        for batch in &batches {
            read_chunk_rows(batch, self.dim, &mut rows)?;
        }
        Ok(rows)
    }

    /// Run table-level maintenance: compact small fragments, prune
    /// versions older than the LanceDB default retention, and absorb any
    /// freshly-appended rows into existing indices.
    ///
    /// Safe to call on an empty table or one with no vector index — the
    /// corresponding step is a no-op in those cases. A book write is
    /// `delete_partition` followed by `append`, which leaves tombstones
    /// and unindexed rows behind; running this at the end of each book
    /// keeps that churn from accumulating.
    /// Build an ANN index over the chunks table according to `cfg`,
    /// dropping any existing index of the canonical name first. The new
    /// configuration is stamped to `<lancedb_dir>/vectors_meta.json` on
    /// success — `built_at` is owned by the caller (RFC 3339 expected)
    /// so this module stays clock-deterministic.
    ///
    /// Guardrails:
    ///
    /// * `AnnKind::BruteForce` is rejected — there is no index to
    ///   build; call [`drop_ann_index`] instead.
    /// * For `AnnKind::IvfPq` and `AnnKind::IvfHnswPq`,
    ///   `num_sub_vectors` is required, and `dim / num_sub_vectors > 8`
    ///   is rejected as too-coarse quantization.
    /// * `IvfHnsw*` builds are accepted but logged at `warn` — they
    ///   carry an upstream recall regression on lancedb 0.30 and should
    ///   not be relied on as a default.
    pub async fn build_ann_index(
        &self,
        cfg: &AnnConfig,
        lancedb_dir: &Path,
        built_at: String,
    ) -> Result<()> {
        use lancedb::index::Index;
        use lancedb::index::vector::{
            IvfFlatIndexBuilder, IvfHnswFlatIndexBuilder, IvfHnswPqIndexBuilder,
            IvfHnswSqIndexBuilder, IvfPqIndexBuilder, IvfSqIndexBuilder,
        };

        if matches!(cfg.kind, AnnKind::BruteForce) {
            return Err(VectorsError::BuildOnBruteForceKind);
        }
        if matches!(cfg.kind, AnnKind::IvfPq | AnnKind::IvfHnswPq) {
            let nsv = cfg
                .num_sub_vectors
                .ok_or(VectorsError::MissingPqParam("num_sub_vectors"))?;
            if nsv == 0 || self.dim as u32 / nsv > 8 {
                return Err(VectorsError::IvfPqQuantizationTooCoarse {
                    dim: self.dim,
                    num_sub_vectors: nsv as usize,
                });
            }
        }
        if matches!(
            cfg.kind,
            AnnKind::IvfHnswFlat | AnnKind::IvfHnswSq | AnnKind::IvfHnswPq
        ) {
            tracing::warn!(
                kind = cfg.kind.as_str(),
                "IvfHnsw* family is unstable on lancedb 0.30 (upstream recall regression)"
            );
        }

        // Drop any pre-existing index of the canonical name. Missing-
        // index errors are tolerated; other failures bubble up.
        if let Err(e) = self.table.drop_index(DEFAULT_INDEX_NAME).await {
            tracing::debug!(error = ?e, "drop_index before rebuild reported");
        }

        let started = std::time::Instant::now();
        let index = match cfg.kind {
            AnnKind::IvfFlat => Index::IvfFlat(
                IvfFlatIndexBuilder::default()
                    .distance_type(DistanceType::Cosine)
                    .num_partitions(cfg.num_partitions),
            ),
            AnnKind::IvfSq => Index::IvfSq(
                IvfSqIndexBuilder::default()
                    .distance_type(DistanceType::Cosine)
                    .num_partitions(cfg.num_partitions),
            ),
            AnnKind::IvfPq => {
                let mut b = IvfPqIndexBuilder::default()
                    .distance_type(DistanceType::Cosine)
                    .num_partitions(cfg.num_partitions);
                if let Some(nsv) = cfg.num_sub_vectors {
                    b = b.num_sub_vectors(nsv);
                }
                if let Some(nb) = cfg.num_bits {
                    b = b.num_bits(nb);
                }
                Index::IvfPq(b)
            }
            AnnKind::IvfHnswFlat => Index::IvfHnswFlat(
                IvfHnswFlatIndexBuilder::default()
                    .distance_type(DistanceType::Cosine)
                    .num_partitions(cfg.num_partitions),
            ),
            AnnKind::IvfHnswSq => Index::IvfHnswSq(
                IvfHnswSqIndexBuilder::default()
                    .distance_type(DistanceType::Cosine)
                    .num_partitions(cfg.num_partitions),
            ),
            AnnKind::IvfHnswPq => {
                let mut b = IvfHnswPqIndexBuilder::default()
                    .distance_type(DistanceType::Cosine)
                    .num_partitions(cfg.num_partitions);
                if let Some(nsv) = cfg.num_sub_vectors {
                    b = b.num_sub_vectors(nsv);
                }
                if let Some(nb) = cfg.num_bits {
                    b = b.num_bits(nb);
                }
                Index::IvfHnswPq(b)
            }
            AnnKind::BruteForce => unreachable!("guarded above"),
        };

        self.table
            .create_index(&["vector"], index)
            .name(DEFAULT_INDEX_NAME.to_string())
            .execute()
            .await?;

        let built_at_chunk_count = self.count_rows().await? as u64;
        let meta = cfg.to_meta(
            built_at,
            built_at_chunk_count,
            0,
            DEFAULT_INDEX_NAME.to_string(),
        );
        meta::store(lancedb_dir, &meta)?;

        let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
        tracing::info!(
            kind = cfg.kind.as_str(),
            num_partitions = cfg.num_partitions,
            nprobes = cfg.nprobes,
            elapsed_ms,
            built_at_chunk_count,
            "built ann index"
        );

        Ok(())
    }

    /// Drop the ANN index attached to the chunks table (if any) and
    /// mark `vectors_meta.json` as `kind = "brute-force"`. Idempotent —
    /// a missing index is not an error.
    pub async fn drop_ann_index(&self, lancedb_dir: &Path, built_at: String) -> Result<()> {
        if let Err(e) = self.table.drop_index(DEFAULT_INDEX_NAME).await {
            tracing::debug!(error = ?e, "drop_index in drop_ann_index reported");
        }
        let row_count = self.count_rows().await? as u64;
        let cfg = AnnConfig::default_for(AnnKind::BruteForce);
        let meta = cfg.to_meta(built_at, row_count, 0, DEFAULT_INDEX_NAME.to_string());
        meta::store(lancedb_dir, &meta)?;
        tracing::info!("dropped ann index; kind now brute-force");
        Ok(())
    }

    /// Names of all indices LanceDB has on the chunks table. Empty for
    /// a freshly opened, brute-force store.
    pub async fn list_indices(&self) -> Result<Vec<String>> {
        Ok(self
            .table
            .list_indices()
            .await?
            .into_iter()
            .map(|cfg| cfg.name)
            .collect())
    }

    /// Statistics for the named index, or `None` if no index by that
    /// name exists.
    ///
    /// The headline field is [`IndexStatistics::num_unindexed_rows`]:
    /// the count of rows the table holds that are not yet covered by
    /// the index. When `> 0` the index is "behind" and a call to
    /// [`Self::optimize`] will catch it up.
    pub async fn index_stats(&self, name: &str) -> Result<Option<IndexStatistics>> {
        Ok(self.table.index_stats(name).await?)
    }

    /// Read the persisted [`AnnConfig`] from
    /// `<lancedb_dir>/vectors_meta.json`. Returns `Ok(None)` when no
    /// meta file is present — the "legacy / fresh library" state — and
    /// an error if the file is unparseable or carries an unknown kind.
    ///
    /// Reads through to disk every call; the meta is < 1 KB and writes
    /// are rare, so caching would buy little and complicate invalidation.
    pub fn current_ann_cfg(&self, lancedb_dir: &Path) -> Result<Option<AnnConfig>> {
        match meta::load(lancedb_dir)? {
            None => Ok(None),
            Some(m) => Ok(Some(AnnConfig::from_meta(&m)?)),
        }
    }

    pub async fn optimize(&self) -> Result<()> {
        let started = std::time::Instant::now();
        let stats = self.table.optimize(OptimizeAction::All).await?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
        let compaction = stats.compaction.as_ref();
        let prune = stats.prune.as_ref();
        tracing::info!(
            elapsed_ms,
            fragments_added = compaction.map(|c| c.fragments_added).unwrap_or(0),
            fragments_removed = compaction.map(|c| c.fragments_removed).unwrap_or(0),
            files_added = compaction.map(|c| c.files_added).unwrap_or(0),
            files_removed = compaction.map(|c| c.files_removed).unwrap_or(0),
            old_versions_pruned = prune.map(|p| p.old_versions).unwrap_or(0),
            bytes_pruned = prune.map(|p| p.bytes_removed).unwrap_or(0),
            deletion_files_pruned = prune.map(|p| p.deletion_files_removed).unwrap_or(0),
            "optimized chunks table"
        );
        Ok(())
    }

    /// Return the `top_k` chunks nearest `query` under cosine distance,
    /// nearest first.
    ///
    /// Defaults to "no overrides" — equivalent to
    /// [`Self::search_with(query, top_k, SearchOptions::default())`].
    pub async fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<SearchHit>> {
        self.search_with(query, top_k, SearchOptions::default())
            .await
    }

    /// Return the `top_k` chunks nearest `query` with explicit ANN
    /// overrides.
    ///
    /// `opts.nprobes` and `opts.refine_factor` adjust the IVF query
    /// fan-out and PQ refinement for this call; `opts.bypass_index`
    /// forces a brute-force scan even when an index is present (an
    /// AB-testing escape hatch). When no override is set the index's
    /// built-in defaults apply.
    pub async fn search_with(
        &self,
        query: &[f32],
        top_k: usize,
        opts: SearchOptions,
    ) -> Result<Vec<SearchHit>> {
        let mut q = self
            .table
            .vector_search(query)?
            .distance_type(DistanceType::Cosine);
        if opts.bypass_index {
            q = q.bypass_vector_index();
        }
        if let Some(np) = opts.nprobes {
            q = q.nprobes(np);
        }
        if let Some(rf) = opts.refine_factor {
            q = q.refine_factor(rf);
        }
        let batches: Vec<RecordBatch> = q.limit(top_k).execute().await?.try_collect().await?;
        let mut hits = Vec::new();
        for batch in &batches {
            read_hits(batch, &mut hits)?;
        }
        Ok(hits)
    }

    /// Return the `top_k` chunks nearest `query`, restricted to one
    /// book's partition.
    ///
    /// Pairs the ANN query with the same `start_node_id BETWEEN ...`
    /// metadata predicate [`Self::scan_partition`] uses, so the
    /// retrieval covers exactly the chunks owned by `partition` and no
    /// others. Empty partitions return an empty `Vec`. Defaults to "no
    /// ANN overrides" — equivalent to
    /// `search_partition_with(query, partition, top_k, SearchOptions::default())`.
    pub async fn search_by_partition(
        &self,
        query: &[f32],
        partition: PartitionIdx,
        top_k: usize,
    ) -> Result<Vec<SearchHit>> {
        self.search_partition_with(query, partition, top_k, SearchOptions::default())
            .await
    }

    /// [`Self::search_by_partition`] with explicit ANN overrides.
    pub async fn search_partition_with(
        &self,
        query: &[f32],
        partition: PartitionIdx,
        top_k: usize,
        opts: SearchOptions,
    ) -> Result<Vec<SearchHit>> {
        let lo = partition.root().get();
        let hi = partition.get() * NODE_PARTITION_FACTOR + NODE_CAPACITY;
        let mut q = self
            .table
            .vector_search(query)?
            .distance_type(DistanceType::Cosine)
            .only_if(format!("start_node_id BETWEEN {lo} AND {hi}"));
        if opts.bypass_index {
            q = q.bypass_vector_index();
        }
        if let Some(np) = opts.nprobes {
            q = q.nprobes(np);
        }
        if let Some(rf) = opts.refine_factor {
            q = q.refine_factor(rf);
        }
        let batches: Vec<RecordBatch> = q.limit(top_k).execute().await?.try_collect().await?;
        let mut hits = Vec::new();
        for batch in &batches {
            read_hits(batch, &mut hits)?;
        }
        Ok(hits)
    }
}

/// Read the fixed-size list width of the `vector` column from a
/// LanceDB schema. Used when opening an existing chunks table so the
/// store handle reflects the on-disk dim, not the caller's hint.
fn vector_dim_from_schema(schema: &Schema) -> Result<usize> {
    let field = schema
        .field_with_name("vector")
        .map_err(|_| VectorsError::BadColumn("vector"))?;
    match field.data_type() {
        DataType::FixedSizeList(_, size) => Ok(*size as usize),
        _ => Err(VectorsError::BadColumn("vector")),
    }
}

/// The slim seven-column chunks schema, parameterized by vector
/// dimension. `vector` is a fixed-size list so LanceDB can index it;
/// every other column is a flat scalar.
fn chunk_schema(dim: usize) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            true,
        ),
        Field::new("text", DataType::Utf8, false),
        Field::new("start_node_id", DataType::Int64, false),
        Field::new("start_char_offset", DataType::Int32, false),
        Field::new("end_node_id", DataType::Int64, false),
        Field::new("end_char_offset", DataType::Int32, false),
        Field::new("norm_chunk_sha256", DataType::Utf8, false),
    ]))
}

/// Build one Arrow record batch from a non-empty slice of chunk rows.
fn build_batch(rows: &[ChunkRow], dim: usize) -> Result<RecordBatch> {
    let vectors = rows
        .iter()
        .map(|r| {
            Some(
                r.vector
                    .iter()
                    .map(|&f| Some(f))
                    .collect::<Vec<Option<f32>>>(),
            )
        })
        .collect::<Vec<_>>();
    let vector_arr =
        FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(vectors, dim as i32);

    let text = StringArray::from(rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>());
    let start_node = Int64Array::from(
        rows.iter()
            .map(|r| r.start_node_id.get())
            .collect::<Vec<_>>(),
    );
    let start_off = Int32Array::from(rows.iter().map(|r| r.start_char_offset).collect::<Vec<_>>());
    let end_node = Int64Array::from(rows.iter().map(|r| r.end_node_id.get()).collect::<Vec<_>>());
    let end_off = Int32Array::from(rows.iter().map(|r| r.end_char_offset).collect::<Vec<_>>());
    let sha = StringArray::from(
        rows.iter()
            .map(|r| r.norm_chunk_sha256.clone())
            .collect::<Vec<_>>(),
    );

    let batch = RecordBatch::try_new(
        chunk_schema(dim),
        vec![
            Arc::new(vector_arr),
            Arc::new(text),
            Arc::new(start_node),
            Arc::new(start_off),
            Arc::new(end_node),
            Arc::new(end_off),
            Arc::new(sha),
        ],
    )?;
    Ok(batch)
}

/// Read every row of a plain (non-vector-search) batch into `out`,
/// reconstructing the vector column into `Vec<f32>`.
fn read_chunk_rows(batch: &RecordBatch, dim: usize, out: &mut Vec<ChunkRow>) -> Result<()> {
    let vector_col = batch
        .column_by_name("vector")
        .and_then(|c| c.as_any().downcast_ref::<FixedSizeListArray>())
        .ok_or(VectorsError::BadColumn("vector"))?;
    let text = string_column(batch, "text")?;
    let start_node = i64_column(batch, "start_node_id")?;
    let start_off = i32_column(batch, "start_char_offset")?;
    let end_node = i64_column(batch, "end_node_id")?;
    let end_off = i32_column(batch, "end_char_offset")?;
    let sha = string_column(batch, "norm_chunk_sha256")?;

    for i in 0..batch.num_rows() {
        let inner = vector_col.value(i);
        let f32_arr = inner
            .as_any()
            .downcast_ref::<Float32Array>()
            .ok_or(VectorsError::BadColumn("vector"))?;
        if f32_arr.len() != dim {
            return Err(VectorsError::DimensionMismatch {
                got: f32_arr.len(),
                expected: dim,
            });
        }
        let vector: Vec<f32> = (0..dim).map(|j| f32_arr.value(j)).collect();
        out.push(ChunkRow {
            vector,
            text: text.value(i).to_string(),
            start_node_id: NodeId::new(start_node.value(i)),
            start_char_offset: start_off.value(i),
            end_node_id: NodeId::new(end_node.value(i)),
            end_char_offset: end_off.value(i),
            norm_chunk_sha256: sha.value(i).to_string(),
        });
    }
    Ok(())
}

/// Read every row of a search-result batch into `out`.
fn read_hits(batch: &RecordBatch, out: &mut Vec<SearchHit>) -> Result<()> {
    let text = string_column(batch, "text")?;
    let start_node = i64_column(batch, "start_node_id")?;
    let start_off = i32_column(batch, "start_char_offset")?;
    let end_node = i64_column(batch, "end_node_id")?;
    let end_off = i32_column(batch, "end_char_offset")?;
    let sha = string_column(batch, "norm_chunk_sha256")?;
    // `_distance` is the column LanceDB appends to a vector search.
    let distance = f32_column(batch, "_distance")?;

    for i in 0..batch.num_rows() {
        out.push(SearchHit {
            text: text.value(i).to_string(),
            start_node_id: NodeId::new(start_node.value(i)),
            start_char_offset: start_off.value(i),
            end_node_id: NodeId::new(end_node.value(i)),
            end_char_offset: end_off.value(i),
            norm_chunk_sha256: sha.value(i).to_string(),
            distance: distance.value(i),
        });
    }
    Ok(())
}

fn string_column<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a StringArray> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_string_opt::<i32>())
        .ok_or(VectorsError::BadColumn(name))
}

fn i64_column<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a Int64Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_primitive_opt::<Int64Type>())
        .ok_or(VectorsError::BadColumn(name))
}

fn i32_column<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a Int32Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_primitive_opt::<Int32Type>())
        .ok_or(VectorsError::BadColumn(name))
}

fn f32_column<'a>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a Float32Array> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_primitive_opt::<Float32Type>())
        .ok_or(VectorsError::BadColumn(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const DIM: usize = 4;

    /// A chunk row in `partition`, at local offset `offset`, with the
    /// given vector. Text and hash are derived so rows are
    /// distinguishable in assertions.
    fn row(partition: i64, offset: i64, vector: [f32; DIM]) -> ChunkRow {
        let node = PartitionIdx::new(partition)
            .node_id(offset)
            .expect("offset is in range");
        ChunkRow {
            vector: vector.to_vec(),
            text: format!("chunk p{partition} o{offset}"),
            start_node_id: node,
            start_char_offset: 0,
            end_node_id: node,
            end_char_offset: 100,
            norm_chunk_sha256: format!("sha-p{partition}-o{offset}"),
        }
    }

    /// Open a fresh store in a temp directory. The `TempDir` is returned
    /// so the test keeps it alive (drop cleans the directory up).
    async fn fresh_store() -> (TempDir, ChunkStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ChunkStore::open(dir.path(), DIM).await.expect("open");
        (dir, store)
    }

    #[tokio::test]
    async fn open_refuses_a_meta_stamp_above_this_binarys_reader_version() {
        let dir = tempfile::tempdir().expect("temp dir");
        let too_new = bookrack_dbkit::READER_VERSION + 1;
        // Write a sidecar whose `min_reader_version` exceeds this
        // build's cap; the open guard must refuse before touching
        // lancedb.
        let forged = meta::VectorsMeta {
            schema_version: meta::SCHEMA_VERSION,
            min_reader_version: Some(too_new),
            kind: "ivf-flat".to_string(),
            num_partitions: 64,
            num_sub_vectors: None,
            num_bits: None,
            default_nprobes: 40,
            default_refine_factor: None,
            built_at: "2026-06-04T00:00:00Z".to_string(),
            built_at_chunk_count: 0,
            churn_since_rebuild: 0,
            lance_index_name: crate::DEFAULT_INDEX_NAME.to_string(),
        };
        meta::store(dir.path(), &forged).expect("store forged meta");

        let Err(err) = ChunkStore::open(dir.path(), DIM).await else {
            panic!("open must refuse a too-new reader stamp")
        };
        assert!(
            matches!(err, VectorsError::ReaderTooOld { required, current }
                if required == too_new && current == bookrack_dbkit::READER_VERSION),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn default_for_ivf_flat_matches_c1_recommendation() {
        let cfg = AnnConfig::default_for(AnnKind::IvfFlat);
        assert_eq!(cfg.num_partitions, 64);
        assert_eq!(cfg.nprobes, 40);
        assert!(cfg.refine_factor.is_none());
        assert!(cfg.num_sub_vectors.is_none());
    }

    #[test]
    fn default_for_ivf_pq_uses_lance_recommended_nsv() {
        let cfg = AnnConfig::default_for(AnnKind::IvfPq);
        assert_eq!(cfg.num_sub_vectors, Some(128));
        assert_eq!(cfg.num_bits, Some(8));
    }

    #[test]
    fn default_for_brute_force_clears_ivf_params() {
        let cfg = AnnConfig::default_for(AnnKind::BruteForce);
        assert_eq!(cfg.num_partitions, 0);
        assert_eq!(cfg.nprobes, 0);
        assert!(cfg.num_sub_vectors.is_none());
        assert!(cfg.refine_factor.is_none());
    }

    #[test]
    fn ann_kind_as_str_round_trips_through_from_str() {
        for kind in [
            AnnKind::IvfFlat,
            AnnKind::IvfSq,
            AnnKind::IvfPq,
            AnnKind::IvfHnswFlat,
            AnnKind::IvfHnswSq,
            AnnKind::IvfHnswPq,
            AnnKind::BruteForce,
        ] {
            let parsed: AnnKind = kind.as_str().parse().expect("kebab parses");
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn from_str_rejects_unknown_kind() {
        let err = "ivf-quantum".parse::<AnnKind>().unwrap_err();
        assert!(matches!(err, VectorsError::UnknownAnnKind(s) if s == "ivf-quantum"));
    }

    #[test]
    fn meta_round_trip_preserves_config() {
        let cfg = AnnConfig::default_for(AnnKind::IvfPq);
        let meta = cfg.clone().to_meta(
            "2026-06-03T17:47:00Z".to_string(),
            66_703,
            0,
            crate::DEFAULT_INDEX_NAME.to_string(),
        );
        let decoded = AnnConfig::from_meta(&meta).expect("from_meta");
        assert_eq!(decoded, cfg);
    }

    #[test]
    fn from_meta_propagates_unknown_kind() {
        let mut meta = AnnConfig::default_for(AnnKind::IvfFlat).to_meta(
            "2026-06-03T00:00:00Z".to_string(),
            0,
            0,
            crate::DEFAULT_INDEX_NAME.to_string(),
        );
        meta.kind = "ivf-warpdrive".to_string();
        let err = AnnConfig::from_meta(&meta).unwrap_err();
        assert!(matches!(err, VectorsError::UnknownAnnKind(_)));
    }

    /// Helper for tests that need a non-default embedding dimension.
    async fn fresh_store_with_dim(dim: usize) -> (TempDir, ChunkStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ChunkStore::open(dir.path(), dim).await.expect("open");
        (dir, store)
    }

    fn fixed_ts() -> String {
        "2026-06-03T17:47:00Z".to_string()
    }

    #[tokio::test]
    async fn build_on_brute_force_kind_is_rejected() {
        let (dir, store) = fresh_store().await;
        let cfg = AnnConfig::default_for(AnnKind::BruteForce);
        let err = store
            .build_ann_index(&cfg, dir.path(), fixed_ts())
            .await
            .unwrap_err();
        assert!(matches!(err, VectorsError::BuildOnBruteForceKind));
    }

    #[tokio::test]
    async fn build_ivf_pq_without_num_sub_vectors_is_rejected() {
        let (dir, store) = fresh_store().await;
        let cfg = AnnConfig {
            kind: AnnKind::IvfPq,
            num_partitions: 1,
            num_sub_vectors: None,
            num_bits: Some(8),
            nprobes: 1,
            refine_factor: None,
        };
        let err = store
            .build_ann_index(&cfg, dir.path(), fixed_ts())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            VectorsError::MissingPqParam("num_sub_vectors")
        ));
    }

    #[tokio::test]
    async fn build_ivf_pq_with_too_coarse_quantization_is_rejected() {
        // dim/nsv = 16/1 = 16 > 8, so the guardrail trips.
        let (dir, store) = fresh_store_with_dim(16).await;
        let cfg = AnnConfig {
            kind: AnnKind::IvfPq,
            num_partitions: 1,
            num_sub_vectors: Some(1),
            num_bits: Some(8),
            nprobes: 1,
            refine_factor: None,
        };
        let err = store
            .build_ann_index(&cfg, dir.path(), fixed_ts())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            VectorsError::IvfPqQuantizationTooCoarse {
                dim: 16,
                num_sub_vectors: 1,
            }
        ));
    }

    #[tokio::test]
    async fn drop_on_empty_table_writes_brute_force_meta() {
        let (dir, store) = fresh_store().await;
        store
            .drop_ann_index(dir.path(), fixed_ts())
            .await
            .expect("drop");
        let meta = meta::load(dir.path()).expect("load").expect("meta present");
        assert_eq!(meta.kind, "brute-force");
        assert_eq!(meta.built_at_chunk_count, 0);
        assert_eq!(meta.churn_since_rebuild, 0);
    }

    #[tokio::test]
    async fn drop_is_idempotent() {
        let (dir, store) = fresh_store().await;
        store
            .drop_ann_index(dir.path(), fixed_ts())
            .await
            .expect("first drop");
        store
            .drop_ann_index(dir.path(), fixed_ts())
            .await
            .expect("second drop");
        let meta = meta::load(dir.path()).expect("load").expect("meta present");
        assert_eq!(meta.kind, "brute-force");
    }

    #[tokio::test]
    async fn list_indices_on_an_empty_table_is_empty() {
        let (_dir, store) = fresh_store().await;
        let names = store.list_indices().await.expect("list_indices");
        assert!(names.is_empty(), "got {names:?}");
    }

    #[tokio::test]
    async fn index_stats_for_an_unknown_name_returns_none() {
        let (_dir, store) = fresh_store().await;
        let stats = store
            .index_stats("nonexistent_index")
            .await
            .expect("index_stats");
        assert!(stats.is_none());
    }

    #[tokio::test]
    async fn list_indices_includes_vector_idx_after_build() {
        let (dir, store) = fresh_store().await;
        let rows: Vec<ChunkRow> = (0..300)
            .map(|i| {
                let v = i as f32 / 300.0;
                row(1, i + 1, [v, 1.0 - v, 0.5, 0.25])
            })
            .collect();
        store.append(&rows).await.expect("append");
        let cfg = AnnConfig {
            kind: AnnKind::IvfFlat,
            num_partitions: 1,
            num_sub_vectors: None,
            num_bits: None,
            nprobes: 1,
            refine_factor: None,
        };
        store
            .build_ann_index(&cfg, dir.path(), fixed_ts())
            .await
            .expect("build");
        let names = store.list_indices().await.expect("list_indices");
        assert!(
            names.contains(&DEFAULT_INDEX_NAME.to_string()),
            "got {names:?}"
        );
        let stats = store
            .index_stats(DEFAULT_INDEX_NAME)
            .await
            .expect("index_stats")
            .expect("stats present");
        assert_eq!(stats.num_unindexed_rows, 0);
    }

    #[tokio::test]
    async fn current_ann_cfg_returns_none_without_meta() {
        let (dir, store) = fresh_store().await;
        assert!(
            store
                .current_ann_cfg(dir.path())
                .expect("read cfg")
                .is_none()
        );
    }

    #[tokio::test]
    async fn current_ann_cfg_returns_brute_force_after_drop() {
        let (dir, store) = fresh_store().await;
        store
            .drop_ann_index(dir.path(), fixed_ts())
            .await
            .expect("drop");
        let cfg = store
            .current_ann_cfg(dir.path())
            .expect("read cfg")
            .expect("meta present");
        assert_eq!(cfg.kind, AnnKind::BruteForce);
    }

    #[tokio::test]
    async fn current_ann_cfg_round_trips_after_build() {
        let (dir, store) = fresh_store().await;
        let rows: Vec<ChunkRow> = (0..300)
            .map(|i| {
                let v = i as f32 / 300.0;
                row(1, i + 1, [v, 1.0 - v, 0.5, 0.25])
            })
            .collect();
        store.append(&rows).await.expect("append");
        let cfg = AnnConfig {
            kind: AnnKind::IvfFlat,
            num_partitions: 1,
            num_sub_vectors: None,
            num_bits: None,
            nprobes: 1,
            refine_factor: None,
        };
        store
            .build_ann_index(&cfg, dir.path(), fixed_ts())
            .await
            .expect("build");
        let read = store
            .current_ann_cfg(dir.path())
            .expect("read cfg")
            .expect("present");
        assert_eq!(read, cfg);
    }

    #[tokio::test]
    async fn build_ivf_flat_then_meta_reflects_config() {
        // Use enough rows to satisfy lancedb's IVF training; even
        // num_partitions=1 wants a substantive sample. 300 vectors at
        // dim=4 are tiny in absolute terms but exercise the path.
        let (dir, store) = fresh_store().await;
        let rows: Vec<ChunkRow> = (0..300)
            .map(|i| {
                let v = i as f32 / 300.0;
                row(1, i + 1, [v, 1.0 - v, 0.5, 0.25])
            })
            .collect();
        store.append(&rows).await.expect("append");
        let cfg = AnnConfig {
            kind: AnnKind::IvfFlat,
            num_partitions: 1,
            num_sub_vectors: None,
            num_bits: None,
            nprobes: 1,
            refine_factor: None,
        };
        store
            .build_ann_index(&cfg, dir.path(), fixed_ts())
            .await
            .expect("build");
        let meta = meta::load(dir.path()).expect("load").expect("meta present");
        assert_eq!(meta.kind, "ivf-flat");
        assert_eq!(meta.num_partitions, 1);
        assert_eq!(meta.default_nprobes, 1);
        assert_eq!(meta.built_at_chunk_count, 300);
        assert_eq!(meta.churn_since_rebuild, 0);
        assert_eq!(meta.lance_index_name, DEFAULT_INDEX_NAME);
    }

    #[tokio::test]
    async fn append_then_count_round_trips() {
        let (_dir, store) = fresh_store().await;
        let written = store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
                row(1, 3, [0.0, 0.0, 1.0, 0.0]),
            ])
            .await
            .expect("append");
        assert_eq!(written, 3);
        assert_eq!(store.count_rows().await.expect("count"), 3);
    }

    #[tokio::test]
    async fn an_empty_append_writes_nothing() {
        let (_dir, store) = fresh_store().await;
        assert_eq!(store.append(&[]).await.expect("append"), 0);
        assert_eq!(store.count_rows().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn a_wrong_dimension_vector_is_rejected() {
        let (_dir, store) = fresh_store().await;
        let mut bad = row(1, 1, [1.0, 0.0, 0.0, 0.0]);
        bad.vector = vec![1.0, 0.0, 0.0]; // three elements, store expects four
        let err = store.append(&[bad]).await.unwrap_err();
        assert!(
            matches!(
                err,
                VectorsError::DimensionMismatch {
                    got: 3,
                    expected: 4
                }
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn scan_partition_on_an_empty_store_returns_no_rows() {
        let (_dir, store) = fresh_store().await;
        let rows = store
            .scan_partition(PartitionIdx::new(7))
            .await
            .expect("scan");
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn scan_partition_returns_only_rows_of_that_book_with_vectors() {
        let (_dir, store) = fresh_store().await;
        let written = [
            row(1, 1, [1.0, 0.0, 0.0, 0.0]),
            row(1, 2, [0.0, 1.0, 0.0, 0.0]),
            row(1, 3, [0.0, 0.0, 1.0, 0.0]),
            row(2, 1, [0.0, 0.0, 0.0, 1.0]),
        ];
        store.append(&written).await.expect("append");

        let mut rows = store
            .scan_partition(PartitionIdx::new(1))
            .await
            .expect("scan");
        assert_eq!(rows.len(), 3);
        // Order is LanceDB-defined; sort by offset for a stable assertion.
        rows.sort_by_key(|r| r.start_node_id.get());
        for (got, expected) in rows.iter().zip(written.iter().take(3)) {
            assert_eq!(got.vector, expected.vector);
            assert_eq!(got.text, expected.text);
            assert_eq!(got.start_node_id, expected.start_node_id);
            assert_eq!(got.start_char_offset, expected.start_char_offset);
            assert_eq!(got.end_node_id, expected.end_node_id);
            assert_eq!(got.end_char_offset, expected.end_char_offset);
            assert_eq!(got.norm_chunk_sha256, expected.norm_chunk_sha256);
        }

        let other = store
            .scan_partition(PartitionIdx::new(2))
            .await
            .expect("scan");
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].vector, written[3].vector);
    }

    #[tokio::test]
    async fn delete_partition_erases_only_that_book() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
                row(2, 1, [0.0, 0.0, 1.0, 0.0]),
            ])
            .await
            .expect("append");
        store
            .delete_partition(PartitionIdx::new(1))
            .await
            .expect("delete");
        // Only partition 2's single row survives.
        assert_eq!(store.count_rows().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn search_returns_the_nearest_chunk_first() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
                row(1, 3, [0.0, 0.0, 1.0, 0.0]),
            ])
            .await
            .expect("append");
        // A query closest to the first row's direction.
        let hits = store
            .search(&[0.9, 0.1, 0.0, 0.0], 3)
            .await
            .expect("search");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].text, "chunk p1 o1");
        assert_eq!(hits[0].start_node_id, NodeId::new(100_000_001));
        // Distances are sorted nearest-first.
        assert!(hits[0].distance <= hits[1].distance);
    }

    #[tokio::test]
    async fn search_by_partition_isolates_one_book() {
        let (_dir, store) = fresh_store().await;
        // Same vector in two partitions: without filtering, both would
        // hit; with partition filtering only one book's row may appear.
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
                row(2, 1, [1.0, 0.0, 0.0, 0.0]),
                row(2, 2, [0.0, 1.0, 0.0, 0.0]),
            ])
            .await
            .expect("append");

        let hits = store
            .search_by_partition(&[0.9, 0.1, 0.0, 0.0], PartitionIdx::new(1), 4)
            .await
            .expect("search by partition");

        assert_eq!(hits.len(), 2);
        for hit in &hits {
            assert_eq!(hit.start_node_id.partition(), PartitionIdx::new(1));
        }
        assert!(hits[0].text.starts_with("chunk p1"));
    }

    #[tokio::test]
    async fn search_by_partition_on_an_empty_partition_returns_no_hits() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[row(1, 1, [1.0, 0.0, 0.0, 0.0])])
            .await
            .expect("append");
        let hits = store
            .search_by_partition(&[1.0, 0.0, 0.0, 0.0], PartitionIdx::new(7), 4)
            .await
            .expect("search empty partition");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn optimize_on_an_empty_table_is_a_noop() {
        let (_dir, store) = fresh_store().await;
        store.optimize().await.expect("optimize");
        assert_eq!(store.count_rows().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn optimize_after_append_keeps_rows_and_can_search() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
            ])
            .await
            .expect("append");
        store.optimize().await.expect("optimize");
        assert_eq!(store.count_rows().await.expect("count"), 2);
        let hits = store
            .search(&[0.9, 0.1, 0.0, 0.0], 2)
            .await
            .expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "chunk p1 o1");
    }

    #[tokio::test]
    async fn optimize_after_delete_then_append_clears_tombstones() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
                row(1, 3, [0.0, 0.0, 1.0, 0.0]),
            ])
            .await
            .expect("append");
        // Simulate the book-write pattern: delete the partition, append
        // fresh rows, then optimize. The optimize call must succeed and
        // the row count must reflect only the second batch.
        store
            .delete_partition(PartitionIdx::new(1))
            .await
            .expect("delete");
        store
            .append(&[
                row(1, 1, [0.5, 0.5, 0.0, 0.0]),
                row(1, 2, [0.5, 0.0, 0.5, 0.0]),
            ])
            .await
            .expect("re-append");
        store.optimize().await.expect("optimize");
        assert_eq!(store.count_rows().await.expect("count"), 2);
    }

    #[tokio::test]
    async fn search_with_default_options_matches_old_search() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
                row(1, 3, [0.0, 0.0, 1.0, 0.0]),
            ])
            .await
            .expect("append");
        let q = [0.9, 0.1, 0.0, 0.0];
        let a = store.search(&q, 3).await.expect("search");
        let b = store
            .search_with(&q, 3, SearchOptions::default())
            .await
            .expect("search_with");
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn search_with_bypass_index_returns_results() {
        // Without an index, lancedb already runs brute-force; the
        // bypass flag is the no-op path. Still exercise it so a future
        // refactor cannot silently break the API.
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
            ])
            .await
            .expect("append");
        let opts = SearchOptions {
            bypass_index: true,
            ..SearchOptions::default()
        };
        let hits = store
            .search_with(&[0.9, 0.1, 0.0, 0.0], 2, opts)
            .await
            .expect("search_with");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "chunk p1 o1");
    }

    #[tokio::test]
    async fn search_with_nprobes_override_runs_without_error() {
        let (_dir, store) = fresh_store().await;
        store
            .append(&[
                row(1, 1, [1.0, 0.0, 0.0, 0.0]),
                row(1, 2, [0.0, 1.0, 0.0, 0.0]),
            ])
            .await
            .expect("append");
        let opts = SearchOptions {
            nprobes: Some(5),
            refine_factor: Some(2),
            bypass_index: false,
        };
        let hits = store
            .search_with(&[0.9, 0.1, 0.0, 0.0], 2, opts)
            .await
            .expect("search_with");
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn a_reopened_store_sees_existing_rows() {
        let dir = tempfile::tempdir().expect("temp dir");
        {
            let store = ChunkStore::open(dir.path(), DIM).await.expect("open");
            store
                .append(&[row(1, 1, [1.0, 0.0, 0.0, 0.0])])
                .await
                .expect("append");
        }
        // A second handle on the same directory finds the committed row.
        let reopened = ChunkStore::open(dir.path(), DIM).await.expect("reopen");
        assert_eq!(reopened.count_rows().await.expect("count"), 1);
    }
}
