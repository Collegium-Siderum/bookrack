// SPDX-License-Identifier: Apache-2.0

//! ingest: assemble an [`Extraction`] into the persistent data model.
//!
//! This milestone implements STRUCTURE: turning one extraction into a
//! `corpus.db` node tree — an organizing tree lifted from the table of
//! contents, prose and structural leaves carrying the body text, and the
//! content hashes that key cross-file deduplication. Chunking, embedding
//! and the vector store are later stages and are not wired here.
//!
//! The unit of ingestion is one already-registered intake: the caller
//! supplies its `intake_id` and the [`Extraction`] the `extract` crate
//! produced. The node-id partition is keyed by that intake id, so a
//! re-ingest of the same file reproduces identical ids; STRUCTURE first
//! drops any prior tree for the intake, making the operation idempotent.

mod chunk;
mod dryrun;
mod embed_run;
pub mod envelope;
pub mod ocr;
pub mod rebuild;
pub mod reembed;
pub mod sentences;
mod structure;

pub use bookrack_corpus::IndexStamps;
pub use chunk::{CHUNK_VERSION, ChunkParams, ChunkPlan};
pub use dryrun::{
    BiblioOut, ChunkStatsOut, DryrunBookReport, DryrunParams, DryrunSummary, FieldOut,
    SUPPORTED_EXTENSIONS, StructureOut, TocStatsOut, collect_files, dryrun_book, dryrun_path,
    summarize,
};
pub use embed_run::{EmbedRunReport, embed_book_chunks, now_rfc3339};
pub use envelope::{
    ENVELOPE_FILE_SUFFIX, ENVELOPE_SCHEMA_VERSION, EnvelopeError, ExtractionEnvelope,
    envelope_filename, read_envelope, write_envelope,
};

use std::path::Path;

use bookrack_catalog::{
    ActorKind, BOOK_SCOPE, Catalog, IntakeStatus, NewBookPipelineAudit, NewBookState,
    NewContributor, NewIntake, NewPublicationAttrs, NewReview,
};
use bookrack_config::EmbedConfig;
use bookrack_core::{NodeId, NodeType, PartitionIdx};
use bookrack_corpus::{Corpus, Node};
use bookrack_embed::Embedder;
use bookrack_extract::{ExtractOutcome, Extraction};
use sha2::{Digest, Sha256};
use tracing::Instrument;

/// The index stamps this binary builds an index with.
///
/// The single assembly point for "what this build expects": the model and
/// vector width are runtime values the caller supplies, while the chunk and
/// normalize versions are this binary's compiled-in constants. Both the
/// build-side gate (in [`embed_book_chunks`]) and the serve-side gate (in
/// the query facade) compare against the [`IndexStamps`] this returns.
pub fn current_index_stamps(embed_model: impl Into<String>, vector_dim: u32) -> IndexStamps {
    IndexStamps {
        embed_model: embed_model.into(),
        vector_dim,
        chunk_version: CHUNK_VERSION,
        normalize_version: bookrack_normalize::NORMALIZE_VERSION,
    }
}

/// Tuning parameters for STRUCTURE.
#[derive(Debug, Clone)]
pub struct StructureParams {
    /// Length, in hex characters, of the stable-anchor prefix taken from
    /// each prose leaf's normalized-text hash.
    pub stable_anchor_len: usize,
}

impl Default for StructureParams {
    fn default() -> StructureParams {
        StructureParams {
            stable_anchor_len: 16,
        }
    }
}

/// What one STRUCTURE run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureReport {
    /// The book's root node id (the partition's reserved root offset).
    pub book_root_id: NodeId,
    /// Total nodes written, including the root.
    pub nodes_written: usize,
    /// How many of those nodes are prose leaves.
    pub prose_leaves: usize,
    /// TOC shape statistics, consumed by the metadata audit as a
    /// warning-level signal. Never gates STRUCTURE itself.
    pub toc_stats: TocStats,
}

pub use bookrack_metadata::TocStats;

/// Why an `ingest` operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IngestError {
    /// The corpus layer reported an error — allocation, validation, or
    /// the underlying database.
    #[error("corpus error: {0}")]
    Corpus(#[from] bookrack_corpus::CorpusError),

    /// The extraction yielded no prose leaf, so there is no searchable
    /// body text to ingest.
    #[error("extraction produced no prose leaves")]
    EmptyExtraction,

    /// Reading the source file failed.
    #[error("reading the source file failed: {0}")]
    Io(#[from] std::io::Error),

    /// The `extract` stage failed to parse the source file.
    #[error("extract error: {0}")]
    Extract(#[from] bookrack_extract::ExtractError),

    /// The source has no usable text layer and must go through OCR, which
    /// this pipeline does not do.
    #[error("source needs OCR and cannot be ingested as text: {reason}")]
    NeedsOcr {
        /// Why the text layer was judged unusable.
        reason: String,
    },

    /// The catalog layer reported an error.
    #[error("catalog error: {0}")]
    Catalog(#[from] bookrack_catalog::CatalogError),

    /// The embed client reported an error that could not be recovered by
    /// shrinking the batch.
    #[error("embed error: {0}")]
    Embed(#[from] bookrack_embed::EmbedError),

    /// The vector store reported an error.
    #[error("vector store error: {0}")]
    Vectors(#[from] bookrack_vectors::VectorsError),

    /// The embedder returned no vector for a non-empty batch, so the
    /// embedding dimension could not be determined.
    #[error("the embedder returned no vector for a non-empty batch")]
    EmptyEmbedding,

    /// A driver was asked to operate on an intake id no row exists for.
    #[error("no intake with id {0}")]
    UnknownIntake(i64),

    /// A reembed target intake is not in the `Embedded` lifecycle state,
    /// so its chunks are not on disk to reembed.
    #[error("intake {0} is not in the Embedded state; cannot reembed")]
    IntakeNotEmbedded(i64),

    /// The source PDF an OCR product names is already registered in a
    /// non-`NeedsOcr` state, so routing it through OCR would conflict
    /// with the path it has already taken (extract, embed, or abort).
    /// The intake's existing status is reported as a stable string.
    #[error("source PDF intake {intake_id} is in {status}, not needs_ocr")]
    OcrSourceStatusMismatch {
        /// The conflicting intake id.
        intake_id: i64,
        /// The intake's existing status as the database string form.
        status: &'static str,
    },

    /// The OCR product's sheet set is missing pages the source said
    /// should be present, and the caller did not opt into a partial
    /// ingest. The missing sheet numbers are 1-based.
    #[error("OCR product is missing pages: {missing:?}")]
    OcrPagesMissing {
        /// The sheets the source declared (1-based) that the OCR
        /// product did not cover.
        missing: Vec<u32>,
    },

    /// The OCR product covers sheets the source PDF does not have,
    /// which means the two were not produced from the same source.
    /// Always an error — there is no `--allow-partial`-equivalent
    /// route to ingest an OCR product paired with the wrong PDF.
    #[error("OCR product carries pages not in the source: {excess:?}")]
    OcrPagesExcess {
        /// The 1-based sheets in the OCR product that the source did
        /// not declare.
        excess: Vec<u32>,
    },
}

/// A fallible `ingest` operation.
pub type Result<T> = std::result::Result<T, IngestError>;

/// Build the corpus node tree for one extraction and write it.
///
/// `intake_id` names an already-registered intake; it keys the node-id
/// partition. `book_root_type` is the organizing type of the book root
/// (typically [`NodeType::Work`]).
///
/// The operation is idempotent: any tree previously written for this
/// intake is dropped before the new one is allocated, so re-ingesting a
/// file replaces its tree rather than duplicating or colliding with it.
/// An extraction with no prose leaf is rejected with
/// [`IngestError::EmptyExtraction`] before the corpus is touched.
pub fn ingest_structure(
    corpus: &mut Corpus,
    intake_id: i64,
    book_root_type: NodeType,
    extraction: &Extraction,
    params: &StructureParams,
) -> Result<StructureReport> {
    // Plan first, while nothing is written: an empty extraction must not
    // drop an existing tree.
    let plan = structure::plan_tree(book_root_type, extraction, params)?;
    let prose_leaves = plan.prose_leaves;
    let child_count = plan.child_count();
    let toc_stats = structure::toc_stats(extraction);

    let idx = PartitionIdx::new(intake_id);
    corpus.drop_partition(idx)?;
    let partition = corpus.allocate_partition(intake_id)?;
    let ids = corpus.allocate_node_ids(idx, child_count)?;

    let nodes = plan.into_new_nodes(partition.book_root_id, &ids);
    let nodes_written = nodes.len();
    corpus.insert_nodes(&nodes)?;

    Ok(StructureReport {
        book_root_id: partition.book_root_id,
        nodes_written,
        prose_leaves,
        toc_stats,
    })
}

/// Plan the chunks for one already-ingested book.
///
/// Reads the book's prose leaves from `corpus`, orders them by document
/// position, and chunks them with the pure [`chunk`] planner. The chunks
/// are the embed stage's input; this only reads — nothing is written.
pub fn plan_book_chunks(
    corpus: &Corpus,
    book_root_id: NodeId,
    params: &ChunkParams,
) -> Result<Vec<ChunkPlan>> {
    let nodes = corpus.book_nodes(book_root_id)?;
    let mut leaves: Vec<&Node> = nodes
        .iter()
        .filter(|n| n.node_type.is_prose_leaf())
        .collect();
    leaves.sort_by_key(|n| n.toc_lo.unwrap_or_else(|| n.node_id.get()));
    let chunk_leaves: Vec<chunk::ChunkLeaf<'_>> = leaves
        .iter()
        .filter_map(|n| {
            n.text_content.as_deref().map(|text| chunk::ChunkLeaf {
                node_id: n.node_id,
                parent_id: n.parent_id,
                text,
            })
        })
        .collect();
    Ok(chunk::plan_chunks(&chunk_leaves, params))
}

/// Tuning for one [`ingest_book`] run: the STRUCTURE, CHUNK, and EMBED
/// knobs, gathered so a caller passes one value.
#[derive(Debug, Clone, Default)]
pub struct IngestParams {
    /// STRUCTURE tuning.
    pub structure: StructureParams,
    /// CHUNK tuning (content-identity, frozen with `CHUNK_VERSION`).
    pub chunk: ChunkParams,
    /// EMBED tuning (operational; see [`EmbedConfig::from_env`]).
    pub embed: EmbedConfig,
    /// When true, an audit verdict of `needs_work` parks the book in
    /// the metadata stage instead of running CHUNK and EMBED. The
    /// caller resumes the run later with [`resume_from_chunk`].
    /// Off by default: the audit is purely advisory.
    pub hold_for_metadata: bool,
    /// Runtime-loaded rule set the metadata audit consults. Defaults
    /// to an empty set; load real rules from
    /// `Config::audit_rules_dir()` and assign.
    pub audit_rules: bookrack_metadata::AuditRules,
    /// Active audit profile. Drives the toggle and threshold reads in
    /// the EPUB / TXT half-rules, the filename parser, and the
    /// downstream audit. Defaults to the shipped profile so an
    /// embedder that does not set it sees the pre-profile behaviour.
    pub audit_profile: bookrack_metadata::AuditProfile,
    /// When true, run the full pipeline even if the source's
    /// `source_sha256` is already on file and the stored
    /// `extractor_version` / `embed_model` stamps match this binary's.
    /// Off by default — a re-ingest of an up-to-date source returns a
    /// no-op report instead of re-extracting and re-embedding.
    pub force: bool,
}

/// What one [`ingest_book`] run produced.
#[derive(Debug, Clone)]
pub struct IngestReport {
    /// The intake the file registered as.
    pub intake_id: i64,
    /// The book's root node id.
    pub book_root_id: NodeId,
    /// Total corpus nodes written, including the root. Zero on a
    /// `no_op` run.
    pub nodes_written: usize,
    /// How many of those nodes are prose leaves. Zero on a `no_op` run.
    pub prose_leaves: usize,
    /// How many chunk rows were embedded and written to the vector
    /// store. Zero on a `no_op` run.
    pub chunks_written: usize,
    /// Whether the file was already registered (idempotent re-ingest).
    pub already_registered: bool,
    /// True when the source was already on file and every recorded
    /// stamp matched, so the pipeline returned without re-extracting,
    /// re-chunking, or re-embedding. The `nodes_written`,
    /// `prose_leaves`, and `chunks_written` counts are all zero.
    pub no_op: bool,
    /// True when the caller asked for [`IngestParams::force`] and the
    /// pipeline therefore ran end-to-end even though the source was
    /// already on file. Mutually exclusive with `no_op`.
    pub forced: bool,
    /// The audit's plausibility verdict for the book's effective
    /// metadata, as a stable token (`clean` / `needs_work`). `None`
    /// when no audit ran — either the active profile disabled it
    /// (`trust-source`) or the book was held at the metadata gate
    /// before the audit completed. Lets the caller surface a
    /// `needs_work` book to the operator from the run summary instead
    /// of leaving the signal in stderr only.
    pub audit_verdict: Option<String>,
    /// The audit's confidence in that verdict (`high` / `medium` /
    /// `low`), paired with `audit_verdict`. `None` whenever
    /// `audit_verdict` is `None`.
    pub audit_confidence: Option<String>,
}

/// Ingest one source file end to end: extract it, register it, build its
/// corpus tree, chunk it, embed the chunks, and write them to the dense
/// store.
///
/// The whole-file SHA-256 keys the intake, so re-ingesting the same file
/// reuses its intake and replaces its corpus tree and vector rows rather
/// than duplicating them. `lancedb_dir` is the vector store directory;
/// `embedder` turns chunk text into vectors. A file whose text layer is
/// unusable yields [`IngestError::NeedsOcr`].
#[tracing::instrument(
    name = "book",
    skip_all,
    fields(file = %file.display(), intake_id = tracing::field::Empty)
)]
pub async fn ingest_book<E: Embedder>(
    file: &Path,
    corpus: &mut Corpus,
    catalog: &mut Catalog,
    lancedb_dir: &Path,
    books_dir: &Path,
    embedder: &E,
    params: &IngestParams,
) -> Result<IngestReport> {
    // One run id ties every audit row from this invocation together; the
    // whole-file hash anchors the rows to a source that survives deletion.
    let bytes = std::fs::read(file)?;
    let source_sha = sha256_hex(&bytes);
    let run_id = new_run_id(&source_sha);

    // True-idempotent fast path: if the source is already on file, the
    // intake reached `Embedded`, and every stamp this binary would write
    // matches what is already stored, the run has nothing left to do.
    // The caller can override with [`IngestParams::force`] to re-extract
    // and re-embed regardless.
    if !params.force
        && let Some(report) = noop_if_up_to_date(catalog, &source_sha, &params.embed.model)?
    {
        tracing::info!(
            intake_id = report.intake_id,
            "ingest noop: source unchanged and stamps current",
        );
        return Ok(report);
    }

    // EXTRACT.
    let started = std::time::Instant::now();
    let extracted = tracing::info_span!("operation", stage = "extract")
        .in_scope(|| bookrack_extract::extract(file, &params.audit_profile.extract));
    let extraction = match extracted {
        Ok(ExtractOutcome::Extracted(extraction)) => extraction,
        Ok(ExtractOutcome::NeedsOcr { reason }) => {
            audit(
                catalog,
                &run_id,
                &source_sha,
                None,
                "extract",
                "extract",
                "skipped",
                started,
                None,
                Some(&reason),
            );
            return Err(IngestError::NeedsOcr { reason });
        }
        Err(e) => {
            audit(
                catalog,
                &run_id,
                &source_sha,
                None,
                "extract",
                "extract",
                "fail",
                started,
                None,
                Some(&e.to_string()),
            );
            return Err(e.into());
        }
    };
    let adapter = extraction.provenance.adapter.clone();
    audit(
        catalog,
        &run_id,
        &source_sha,
        None,
        "extract",
        "extract",
        "ok",
        started,
        None,
        None,
    );
    tracing::info!(adapter = %adapter, "extracted source file");

    // Register the file, keyed idempotently on its whole-file hash.
    // `original_path` is recorded for forensics and for the search-layer
    // breadcrumb fallback when no title is known; the column may be
    // null on pre-existing rows.
    let registration = catalog.register_intake(
        &NewIntake::new(source_sha.clone())
            .format(adapter.clone())
            .byte_size(bytes.len() as i64)
            .original_path(file.to_string_lossy().into_owned()),
    )?;
    let already_registered = !registration.is_new();
    let intake_id = registration.intake().intake_id;
    // Now that the intake id is known, record it on the book span so every
    // event under this run is attributable to one book.
    tracing::Span::current().record("intake_id", intake_id);
    tracing::info!(intake_id, already_registered, "registered intake");

    // Stamp the extraction provenance, so a later re-extraction can tell
    // whether this book's partition is stale.
    catalog.set_extraction(
        intake_id,
        &extraction.provenance.adapter,
        extraction.provenance.extractor_version,
    )?;
    // The status track is `Pending` (set by `register_intake`) →
    // `Extracted` here → `Embedded` after the embed run completes.
    catalog.set_intake_status(intake_id, IntakeStatus::Extracted)?;

    // Cache the post-EXTRACT Extraction as a v1 envelope in the opaque
    // store and record its path in `intake.stored_path`. Failure is
    // logged but not fatal: the envelope is a rebuild cache, not the
    // source of truth.
    let envelope_path = books_dir.join(envelope::envelope_filename(intake_id));
    match envelope::write_envelope(&envelope_path, &extraction, intake_id, &source_sha) {
        Ok(()) => {
            if let Err(err) =
                catalog.set_stored_path(intake_id, envelope_path.to_string_lossy().as_ref())
            {
                tracing::warn!(
                    intake_id,
                    error = %err,
                    "failed to record stored_path; rebuild path unavailable for this intake"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                intake_id,
                error = %err,
                "failed to write extraction envelope; rebuild path unavailable for this intake"
            );
        }
    }

    // STRUCTURE.
    let started = std::time::Instant::now();
    let structure = match tracing::info_span!("operation", stage = "structure").in_scope(|| {
        ingest_structure(
            corpus,
            intake_id,
            NodeType::Work,
            &extraction,
            &params.structure,
        )
    }) {
        Ok(structure) => structure,
        Err(e) => {
            audit(
                catalog,
                &run_id,
                &source_sha,
                None,
                "structure",
                "structure",
                "fail",
                started,
                None,
                Some(&e.to_string()),
            );
            return Err(e);
        }
    };
    let book_root_id = structure.book_root_id.get();
    let metric = format!(
        r#"{{"nodes":{},"prose_leaves":{}}}"#,
        structure.nodes_written, structure.prose_leaves
    );
    audit(
        catalog,
        &run_id,
        &source_sha,
        Some(book_root_id),
        "structure",
        "structure",
        "ok",
        started,
        Some(metric),
        None,
    );
    tracing::info!(
        nodes = structure.nodes_written,
        prose_leaves = structure.prose_leaves,
        "built corpus tree"
    );
    let parsed_at = catalog.now_iso()?;
    set_state(
        catalog,
        NewBookState::new(book_root_id, intake_id, "structure").parsed_at(&parsed_at),
    );

    // METADATA (non-blocking): seed the publication-attrs base from the
    // extracted biblio, run the deterministic audit over the resulting
    // effective record, and persist a `pending` review row plus the
    // confidence rollup. The audit never gates the pipeline and never
    // marks a record reviewed: it only assesses how plausible the
    // metadata looks. A human or LLM must `metadata approve`,
    // `metadata ack`, or `metadata reject` to advance status.
    let source_stem = file.file_stem().and_then(|s| s.to_str());
    let filename_biblio = source_stem
        .map(|stem| bookrack_metadata::parse_filename(stem, &params.audit_profile.filename_parser));
    let outcome = run_metadata_substep(
        catalog,
        intake_id,
        book_root_id,
        &extraction,
        &structure.toc_stats,
        source_stem,
        filename_biblio.as_ref(),
        &params.audit_rules,
        &params.audit_profile,
        &run_id,
        &source_sha,
    );
    let needs_work = outcome.as_ref().is_some_and(|o| o.verdict == "needs_work");
    let (audit_verdict, audit_confidence) = match &outcome {
        Some(o) => (Some(o.verdict.clone()), Some(o.confidence.clone())),
        None => (None, None),
    };

    // Optional hold gate: when the caller asked for it AND the audit
    // flagged the record, park the book in the metadata stage and
    // hand control back. CHUNK/EMBED run on a later `advance` call.
    if params.hold_for_metadata && needs_work {
        set_state(
            catalog,
            NewBookState::new(book_root_id, intake_id, "metadata").parsed_at(&parsed_at),
        );
        tracing::info!(
            intake_id,
            "held at metadata: --hold-for-metadata is on and verdict is needs_work"
        );
        return Ok(IngestReport {
            intake_id,
            book_root_id: structure.book_root_id,
            nodes_written: structure.nodes_written,
            prose_leaves: structure.prose_leaves,
            chunks_written: 0,
            already_registered,
            no_op: false,
            forced: params.force,
            audit_verdict,
            audit_confidence,
        });
    }

    let embed = resume_from_chunk(
        corpus,
        catalog,
        lancedb_dir,
        embedder,
        params,
        intake_id,
        structure.book_root_id,
        &run_id,
        &source_sha,
        &parsed_at,
    )
    .await?;

    Ok(IngestReport {
        intake_id,
        book_root_id: structure.book_root_id,
        nodes_written: structure.nodes_written,
        prose_leaves: structure.prose_leaves,
        chunks_written: embed.chunks_written,
        already_registered,
        no_op: false,
        forced: params.force,
        audit_verdict,
        audit_confidence,
    })
}

/// Inspect the catalog and book state to decide whether a re-ingest of
/// `source_sha` would do any new work. Returns `Some(report)` if every
/// stamp matches, in which case the caller should return the report
/// immediately; returns `None` to fall through to the full pipeline.
fn noop_if_up_to_date(
    catalog: &Catalog,
    source_sha: &str,
    embed_model: &str,
) -> Result<Option<IngestReport>> {
    let Some(intake) = catalog.intake_by_sha(source_sha)? else {
        return Ok(None);
    };
    if intake.status != IntakeStatus::Embedded {
        return Ok(None);
    }
    if intake.extractor_version != bookrack_extract::EXTRACTOR_VERSION {
        return Ok(None);
    }
    let book_root_id = PartitionIdx::new(intake.intake_id).root();
    let Some(state) = catalog.book_state(book_root_id.get())? else {
        return Ok(None);
    };
    if state.embed_model.as_deref() != Some(embed_model) {
        return Ok(None);
    }
    // Surface the audit outcome stored on the row at the previous
    // ingest so a noop run still tells the operator whether the book
    // landed `clean` or `needs_work`.
    let attrs = catalog.publication_attrs(intake.intake_id, BOOK_SCOPE)?;
    let audit_verdict = attrs.as_ref().and_then(|a| a.audit_verdict.clone());
    let audit_confidence = attrs.as_ref().and_then(|a| a.confidence.clone());
    Ok(Some(IngestReport {
        intake_id: intake.intake_id,
        book_root_id,
        nodes_written: 0,
        prose_leaves: 0,
        chunks_written: 0,
        already_registered: true,
        no_op: true,
        forced: false,
        audit_verdict,
        audit_confidence,
    }))
}

/// Run CHUNK and EMBED for a book whose corpus tree is already in
/// place, then mark the intake `Embedded`. Shared by the steady-state
/// [`ingest_book`] path and the `metadata advance` resume path, so a
/// book held at the metadata gate finishes through the same
/// CHUNK/EMBED code as a non-held book.
///
/// `parsed_at` is the timestamp [`ingest_book`] stamped on the book
/// state when STRUCTURE completed; the resume preserves it rather
/// than minting a new one.
///
/// The function does **not** rebuild the tree — it walks the existing
/// prose leaves out of `corpus` and feeds them straight into the
/// chunker. A book whose STRUCTURE never ran first cannot be advanced
/// through this entry point.
#[allow(clippy::too_many_arguments)]
pub async fn resume_from_chunk<E: Embedder>(
    corpus: &mut Corpus,
    catalog: &mut Catalog,
    lancedb_dir: &Path,
    embedder: &E,
    params: &IngestParams,
    intake_id: i64,
    book_root_id: NodeId,
    run_id: &str,
    source_sha: &str,
    parsed_at: &str,
) -> Result<EmbedRunReport> {
    let book_root_raw = book_root_id.get();

    // CHUNK.
    let started = std::time::Instant::now();
    let plans = match tracing::info_span!("operation", stage = "chunk")
        .in_scope(|| plan_book_chunks(corpus, book_root_id, &params.chunk))
    {
        Ok(plans) => plans,
        Err(e) => {
            audit(
                catalog,
                run_id,
                source_sha,
                Some(book_root_raw),
                "chunk",
                "chunk",
                "fail",
                started,
                None,
                Some(&e.to_string()),
            );
            set_state(
                catalog,
                NewBookState::new(book_root_raw, intake_id, "chunk")
                    .parsed_at(parsed_at)
                    .last_error(e.to_string()),
            );
            return Err(e);
        }
    };
    audit(
        catalog,
        run_id,
        source_sha,
        Some(book_root_raw),
        "chunk",
        "chunk",
        "ok",
        started,
        Some(format!(r#"{{"chunks":{}}}"#, plans.len())),
        None,
    );
    tracing::info!(chunks = plans.len(), "planned chunks");

    // EMBED.
    let started = std::time::Instant::now();
    let embed =
        match embed_run::embed_book_chunks(&plans, embedder, corpus, lancedb_dir, &params.embed)
            .instrument(tracing::info_span!("operation", stage = "embed"))
            .await
        {
            Ok(report) => report,
            Err(e) => {
                audit(
                    catalog,
                    run_id,
                    source_sha,
                    Some(book_root_raw),
                    "embed",
                    "embed",
                    "fail",
                    started,
                    None,
                    Some(&e.to_string()),
                );
                set_state(
                    catalog,
                    NewBookState::new(book_root_raw, intake_id, "embed")
                        .parsed_at(parsed_at)
                        .embed_model(&params.embed.model)
                        .last_error(e.to_string()),
                );
                return Err(e);
            }
        };
    let metric = format!(
        r#"{{"chunks":{},"batches":{},"shrink_events":{},"chars":{}}}"#,
        embed.chunks_written, embed.batches, embed.shrink_events, embed.total_chars
    );
    audit(
        catalog,
        run_id,
        source_sha,
        Some(book_root_raw),
        "embed",
        "embed",
        "ok",
        started,
        Some(metric),
        None,
    );
    tracing::info!(
        chunks_written = embed.chunks_written,
        batches = embed.batches,
        shrink_events = embed.shrink_events,
        "embedded chunks"
    );

    catalog.set_intake_status(intake_id, IntakeStatus::Embedded)?;
    let embedded_at = catalog.now_iso()?;
    set_state(
        catalog,
        NewBookState::new(book_root_raw, intake_id, "embed")
            .embed_model(&params.embed.model)
            .parsed_at(parsed_at)
            .embedded_at(&embedded_at),
    );

    Ok(embed)
}

/// Build a per-invocation pipeline run id: a short source-hash prefix for
/// readability, plus a nanosecond timestamp so repeated ingests of the
/// same file stay distinct runs.
pub(crate) fn new_run_id(source_sha: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ingest-{}-{nanos}", &source_sha[..source_sha.len().min(8)])
}

/// Append one pipeline-audit row, best-effort: the audit trail is
/// observability, so a failure to record it is logged and swallowed rather
/// than failing the ingest it describes.
#[allow(clippy::too_many_arguments)]
fn audit(
    catalog: &Catalog,
    run_id: &str,
    source_sha: &str,
    book_root_id: Option<i64>,
    stage: &str,
    sub_step: &str,
    outcome: &str,
    started: std::time::Instant,
    metric_summary: Option<String>,
    error_message: Option<&str>,
) {
    let mut row = NewBookPipelineAudit::new(stage, sub_step, outcome, run_id, ActorKind::Pipeline);
    row.book_root_id = book_root_id;
    row.source_sha256 = Some(source_sha.to_string());
    row.metric_summary = metric_summary;
    row.error_message = error_message.map(str::to_string);
    row.duration_ms = Some(started.elapsed().as_millis() as i64);
    row.actor_detail = Some("ingest".to_string());
    if let Err(e) = catalog.record_pipeline_audit(&row) {
        tracing::warn!(error = %e, stage, "failed to record pipeline audit row");
    }
}

/// How many leading blocks contribute text to the audit's body sample.
const METADATA_BODY_SAMPLE_BLOCKS: usize = 10;
/// Maximum characters in the body sample carried into the audit.
const METADATA_BODY_SAMPLE_CHARS: usize = 4096;
/// Run the non-blocking metadata sub-step in place: seed the
/// publication-attrs base from the extracted [`Biblio`], run the
/// audit over the resulting effective record, and persist the
/// verdict as an advisory `node_reviews` row plus one pipeline-audit
/// row stamped `stage="metadata"`.
///
/// Returns the audit verdict on success so the caller can decide
/// whether to honour an opt-in metadata hold gate; returns `None` on
/// any persistence failure, in which case the gate cannot trip and
/// the run continues. Best-effort, like [`audit`]: a failure to
/// persist the verdict is logged but does not abort the ingest, since
/// the sub-step is consultative and EMBED is unconditional.
/// The audit outcome the metadata sub-step bubbles up: the same
/// `verdict` and `confidence` tokens that get stamped on the
/// publication-attrs row, in stable string form so a downstream
/// renderer can use them without a metadata-crate type dependency.
#[derive(Debug, Clone)]
pub struct AuditOutcome {
    /// Audit verdict token (`clean` or `needs_work`).
    pub verdict: String,
    /// Confidence token (`high` / `medium` / `low`).
    pub confidence: String,
}

#[allow(clippy::too_many_arguments)]
fn run_metadata_substep(
    catalog: &Catalog,
    intake_id: i64,
    book_root_id: i64,
    extraction: &Extraction,
    toc_stats: &TocStats,
    source_stem: Option<&str>,
    filename_biblio: Option<&bookrack_metadata::FilenameBiblio>,
    audit_rules: &bookrack_metadata::AuditRules,
    audit_profile: &bookrack_metadata::AuditProfile,
    run_id: &str,
    source_sha: &str,
) -> Option<AuditOutcome> {
    let started = std::time::Instant::now();

    // Real ingest discards the action trail; only the dryrun consumes it.
    let mut attrs = build_base_attrs(intake_id, extraction, filename_biblio, audit_profile).attrs;
    if let Err(e) = catalog.upsert_publication_attrs(&attrs) {
        tracing::warn!(error = %e, "metadata: failed to seed publication attrs");
        audit(
            catalog,
            run_id,
            source_sha,
            Some(book_root_id),
            "metadata",
            "seed",
            "fail",
            started,
            None,
            Some(&e.to_string()),
        );
        return None;
    }
    write_contributors(catalog, intake_id, &extraction.biblio, filename_biblio);

    // trust-source short-circuit: when the profile disables the audit,
    // record the substep as skipped and write a `pending` review row
    // with a profile-stamped reviewer so a downstream query can tell a
    // "no audit ran" pending from a "audit flagged" pending. No
    // MetadataReport is produced and no audit-derived columns are
    // written.
    if !audit_profile.audit_enabled {
        let reviewer = format!("bookrack-ingest:{}", audit_profile.name);
        if let Err(e) = catalog.upsert_review(
            &NewReview::new(
                intake_id,
                BOOK_SCOPE,
                &reviewer,
                bookrack_catalog::STATUS_PENDING,
            )
            .notes(format!(
                "audit skipped: {} profile disables the metadata audit",
                audit_profile.name,
            )),
        ) {
            tracing::warn!(error = %e, "metadata: failed to write node_reviews row");
        }
        audit(
            catalog,
            run_id,
            source_sha,
            Some(book_root_id),
            "metadata",
            "audit",
            "skipped",
            started,
            None,
            None,
        );
        return None;
    }

    let effective = match catalog.effective_publication_attrs(intake_id, BOOK_SCOPE) {
        Ok(eff) => eff,
        Err(e) => {
            tracing::warn!(error = %e, "metadata: failed to read effective attrs");
            audit(
                catalog,
                run_id,
                source_sha,
                Some(book_root_id),
                "metadata",
                "read_effective",
                "fail",
                started,
                None,
                Some(&e.to_string()),
            );
            return None;
        }
    };

    let body_sample = body_sample(extraction);
    let input = bookrack_metadata::AuditInput {
        biblio: &extraction.biblio,
        provenance: &extraction.provenance,
        effective: &effective,
        toc_stats,
        body_sample: &body_sample,
        total_blocks: extraction.blocks.len(),
        source_stem,
        rules: audit_rules,
    };
    let report = bookrack_metadata::audit(&input, audit_profile);

    // Roll the audit's row-level confidence and verdict back into the
    // base record. The upsert overwrites every column, so the biblio
    // values seeded above are spelled out again to preserve them.
    // Stamping both fields lets `metadata list` and `metadata show`
    // read the same historical audit outcome from the row instead of
    // one view re-running a synthetic audit.
    attrs.confidence = Some(report.confidence.as_str().to_string());
    attrs.audit_verdict = Some(report.verdict.as_token().to_string());
    if let Err(e) = catalog.upsert_publication_attrs(&attrs) {
        tracing::warn!(error = %e, "metadata: failed to write audit rollup");
    }

    let outcome = match report.verdict {
        bookrack_metadata::Verdict::Clean => "ok",
        bookrack_metadata::Verdict::NeedsWork => "partial",
    };
    let metric = audit_metric_summary(&report);

    // Review status reflects human/LLM confirmation, never the audit.
    // The audit's plausibility verdict travels in the notes column for
    // forensic context and through the `book_pipeline_audit` row below;
    // status itself stays at `pending` until a human or LLM advances it.
    // The reviewed_by string carries the active profile name so a query
    // can tell `default` flavour pending from a `strict`-flavour pending
    // or from the audit-skipped `trust-source` flavour written above.
    let reviewer = format!("bookrack-ingest:{}", audit_profile.name);
    if let Err(e) = catalog.upsert_review(
        &NewReview::new(
            intake_id,
            BOOK_SCOPE,
            &reviewer,
            bookrack_catalog::STATUS_PENDING,
        )
        .notes(report_notes(&report)),
    ) {
        tracing::warn!(error = %e, "metadata: failed to write node_reviews row");
    }

    tracing::info!(
        verdict = report.verdict.as_token(),
        confidence = report.confidence.as_str(),
        "metadata audit complete"
    );

    audit(
        catalog,
        run_id,
        source_sha,
        Some(book_root_id),
        "metadata",
        "audit",
        outcome,
        started,
        Some(metric),
        None,
    );
    Some(AuditOutcome {
        verdict: report.verdict.as_token().to_string(),
        confidence: report.confidence.as_str().to_string(),
    })
}

/// Persist contributors from the adapter-extracted biblio and the
/// filename parser into `node_contributors`. The two sources coexist
/// via distinct `origin` values — `"extracted"` for adapter-derived
/// entries, `"extracted-filename"` for the filename parser's tentative
/// author. Ordinals are independent across origins; the
/// `(intake_id, scope, role, ordinal, origin)` UNIQUE constraint lets
/// them share `ordinal = 0` without collision.
///
/// Deduplication across the two sources is a read-side concern: callers
/// fold them via origin precedence (typically `user > extracted >
/// extracted-filename`). The pipeline-owned rows are dropped before
/// the rewrite so a re-ingest replaces the extracted set wholesale,
/// without ever colliding with itself; rows with `origin = 'user'` are
/// left in place.
fn write_contributors(
    catalog: &Catalog,
    intake_id: i64,
    biblio: &bookrack_extract::Biblio,
    filename_biblio: Option<&bookrack_metadata::FilenameBiblio>,
) {
    if let Err(e) = catalog.clear_extracted_contributors(intake_id, BOOK_SCOPE) {
        tracing::warn!(
            error = %e,
            "metadata: failed to clear pipeline-owned contributors before refresh",
        );
    }
    for (ordinal, contrib) in biblio.contributors.iter().enumerate() {
        let name = contrib.name.trim();
        if name.is_empty() {
            continue;
        }
        let new = NewContributor::new(
            intake_id,
            BOOK_SCOPE,
            contrib.role.as_str(),
            ordinal as i64,
            "extracted",
            name,
        );
        if let Err(e) = catalog.add_contributor(&new) {
            tracing::warn!(
                error = %e,
                role = contrib.role.as_str(),
                "metadata: failed to write extracted contributor",
            );
        }
    }
    if let Some(author) = filename_biblio
        .and_then(|fb| fb.author.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let new = NewContributor::new(
            intake_id,
            BOOK_SCOPE,
            "author",
            0,
            "extracted-filename",
            author,
        );
        if let Err(e) = catalog.add_contributor(&new) {
            tracing::warn!(
                error = %e,
                "metadata: failed to write filename-derived author",
            );
        }
    }
}

/// Build the base-layer record for the book root from the extracted
/// biblio, with an optional filename-derived biblio as a strict
/// fallback per field. `source_format` carries the adapter name so
/// the audit's per-format prior can recompute. `source` is stamped
/// `"extracted"` whenever any field came from the adapter, otherwise
/// `"filename"` whenever any field came from the filename parser,
/// otherwise `"extracted"` to match the legacy all-empty case.
///
/// Adapter values take precedence: a non-empty biblio field from
/// extraction wins over the filename value, since the adapter is the
/// authoritative source when it has anything at all to say. The
/// filename only fills the per-field gaps the adapter left behind.
///
/// The struct is returned rather than written so the caller can
/// re-upsert it after the audit, this time carrying the confidence
/// rollup — `upsert_publication_attrs` overwrites every column, so
/// the biblio fields must be re-stated to be preserved.
fn build_base_attrs(
    intake_id: i64,
    extraction: &Extraction,
    filename_biblio: Option<&bookrack_metadata::FilenameBiblio>,
    profile: &bookrack_metadata::AuditProfile,
) -> BaseAttrsOutcome {
    let biblio = &extraction.biblio;
    let mut attrs = NewPublicationAttrs::new(intake_id, BOOK_SCOPE);
    let mut actions: Vec<BaseAttrsAction> = Vec::new();
    attrs.title = biblio.title.clone();
    attrs.subtitle = biblio.subtitle.clone();
    attrs.publisher = biblio.publisher.clone();
    attrs.year = biblio.year.map(|y| y.to_string());
    attrs.isbn = biblio.isbn.clone();
    attrs.series = biblio.series.clone();
    attrs.language = biblio.language.clone();
    if profile.publisher.drop_10digit_isbn_to_filename {
        drop_invalid_extracted_isbn(&mut attrs.isbn, &mut actions);
    }
    if profile.year.cross_field_filename_override {
        drop_stale_extracted_year(
            &mut attrs.year,
            biblio.year_raw.as_deref(),
            filename_biblio,
            &mut actions,
        );
    }
    let extracted_any = attrs.title.is_some()
        || attrs.subtitle.is_some()
        || attrs.publisher.is_some()
        || attrs.year.is_some()
        || attrs.isbn.is_some()
        || attrs.series.is_some()
        || attrs.language.is_some();
    let mut filename_filled_any = false;
    if let Some(fb) = filename_biblio {
        merge_from_filename(
            "title",
            &mut attrs.title,
            fb.title.as_ref(),
            &mut filename_filled_any,
            &mut actions,
        );
        merge_from_filename(
            "publisher",
            &mut attrs.publisher,
            fb.publisher.as_ref(),
            &mut filename_filled_any,
            &mut actions,
        );
        merge_from_filename(
            "year",
            &mut attrs.year,
            fb.year.as_ref(),
            &mut filename_filled_any,
            &mut actions,
        );
        merge_from_filename(
            "isbn",
            &mut attrs.isbn,
            fb.isbn.as_ref(),
            &mut filename_filled_any,
            &mut actions,
        );
        merge_from_filename(
            "series",
            &mut attrs.series,
            fb.series.as_ref(),
            &mut filename_filled_any,
            &mut actions,
        );
    }
    attrs.source = Some(if extracted_any || !filename_filled_any {
        "extracted".to_string()
    } else {
        "filename".to_string()
    });
    attrs.source_format = Some(extraction.provenance.adapter.clone());
    BaseAttrsOutcome { attrs, actions }
}

/// The bundle [`build_base_attrs`] returns: the row that goes to
/// `node_publication_attrs`, plus the trail of program-level overrides
/// applied while assembling it. The real ingest path discards `actions`;
/// the dryrun surfaces them so a JSONL consumer can see which fields
/// were touched by which filter.
pub(crate) struct BaseAttrsOutcome {
    pub attrs: NewPublicationAttrs,
    pub actions: Vec<BaseAttrsAction>,
}

/// A program-level override applied during base-attrs construction —
/// distinct from the audit's per-field flags, which judge plausibility
/// of the surviving values rather than describing how they got there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BaseAttrsAction {
    /// `drop_invalid_extracted_isbn` rejected the adapter's ISBN.
    DropInvalidIsbn,
    /// `drop_stale_extracted_year` rejected the adapter's year as a
    /// timestamp-shape build date.
    DropStaleYear,
    /// `merge_from_filename` filled an adapter-empty field from the
    /// filename parser. The static string is the field name.
    FilenameFallback(&'static str),
}

impl BaseAttrsAction {
    /// A stable, filename-safe token suitable as a histogram key in
    /// `DryrunSummary.base_attrs_action_counts`.
    pub(crate) fn token(&self) -> String {
        match self {
            Self::DropInvalidIsbn => "drop_invalid_isbn".to_string(),
            Self::DropStaleYear => "drop_stale_year".to_string(),
            Self::FilenameFallback(field) => format!("filename_fallback:{field}"),
        }
    }
}

/// Drop an extracted ISBN whose checksum does not validate. The most
/// common shape this filter targets is the ten-digit purely numeric
/// fallback identifier some EPUB toolchains stamp into `<dc:identifier>`
/// when no real ISBN exists; leaving it in place blocks a valid
/// filename-derived ISBN from filling the slot.
fn drop_invalid_extracted_isbn(slot: &mut Option<String>, actions: &mut Vec<BaseAttrsAction>) {
    if let Some(value) = slot.as_deref()
        && !bookrack_metadata::is_valid_isbn(value)
    {
        *slot = None;
        actions.push(BaseAttrsAction::DropInvalidIsbn);
    }
}

/// Drop an extracted year when the raw date string is in timestamp
/// shape (`YYYY-MM-DDThh:mm:ss...`) and the filename parser yields a
/// different year. The timestamp shape is the canonical EPUB build /
/// export-date sentinel, so when filename and `<dc:date>` disagree the
/// filename year is the more conservative pick.
fn drop_stale_extracted_year(
    slot: &mut Option<String>,
    year_raw: Option<&str>,
    filename_biblio: Option<&bookrack_metadata::FilenameBiblio>,
    actions: &mut Vec<BaseAttrsAction>,
) {
    let Some(current) = slot.as_deref() else {
        return;
    };
    let Some(raw) = year_raw else {
        return;
    };
    if !bookrack_metadata::looks_like_timestamp(raw) {
        return;
    }
    let Some(filename) = filename_biblio else {
        return;
    };
    let Some(filename_year) = filename.year.as_deref() else {
        return;
    };
    if filename_year != current {
        *slot = None;
        actions.push(BaseAttrsAction::DropStaleYear);
    }
}

/// Copy `incoming` into `slot` only when `slot` is currently `None`.
/// Records whether the slot was actually filled from the filename so
/// the caller can pick the right `source` tag, and pushes a
/// [`BaseAttrsAction::FilenameFallback`] tagged with the field name when
/// the fill happens.
fn merge_from_filename(
    field: &'static str,
    slot: &mut Option<String>,
    incoming: Option<&String>,
    filled: &mut bool,
    actions: &mut Vec<BaseAttrsAction>,
) {
    if slot.is_some() {
        return;
    }
    if let Some(v) = incoming {
        *slot = Some(v.clone());
        *filled = true;
        actions.push(BaseAttrsAction::FilenameFallback(field));
    }
}

/// Concatenate text from the first few blocks of the extraction,
/// bounded by a character cap, for the audit's language signal.
fn body_sample(extraction: &Extraction) -> String {
    let mut out = String::new();
    for block in extraction.blocks.iter().take(METADATA_BODY_SAMPLE_BLOCKS) {
        for ch in block.text.chars() {
            if out.chars().count() >= METADATA_BODY_SAMPLE_CHARS {
                return out;
            }
            out.push(ch);
        }
        out.push('\n');
    }
    out
}

/// A short, structured summary of the audit, written into
/// `book_pipeline_audit.metric_summary` for diagnostics.
fn audit_metric_summary(report: &bookrack_metadata::MetadataReport) -> String {
    let flagged = report.fields.iter().filter(|f| !f.flags.is_empty()).count();
    format!(
        r#"{{"verdict":"{}","confidence":"{}","fields":{},"flagged":{}}}"#,
        report.verdict.as_token(),
        report.confidence.as_str(),
        report.fields.len(),
        flagged
    )
}

/// A human-facing note for the `node_reviews.notes` column. Carries the
/// audit's plausibility verdict and a comma-separated list of any
/// flagged fields, so a reviewer can see at a glance what the pipeline
/// thought of the record without re-running the audit.
fn report_notes(report: &bookrack_metadata::MetadataReport) -> String {
    let mut flagged: Vec<String> = report
        .fields
        .iter()
        .filter(|f| !f.flags.is_empty())
        .map(|f| f.field.clone())
        .collect();
    let verdict = report.verdict.as_token();
    if flagged.is_empty() {
        return format!("audit verdict={verdict}, all audited fields clean");
    }
    flagged.sort();
    format!("audit verdict={verdict}, flagged: {}", flagged.join(", "))
}

/// Upsert a book's pipeline state, best-effort for the same reason as
/// [`audit`].
pub(crate) fn set_state(catalog: &Catalog, state: NewBookState) {
    if let Err(e) = catalog.upsert_book_state(&state) {
        tracing::warn!(error = %e, "failed to update book state");
    }
}

/// SHA-256 of raw bytes as 64 lowercase hex characters — the whole-file
/// identity anchor an intake registers under.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        write!(hex, "{byte:02x}").expect("writing to a String is infallible");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_extract::{
        Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc, TocEntry,
    };
    use bookrack_normalize::norm_text_sha256;

    fn body(text: &str, unit: u32) -> Block {
        Block {
            kind: BlockKind::Body,
            text: text.to_string(),
            source_unit: unit,
        }
    }

    fn heading(text: &str, level: u8, unit: u32) -> Block {
        Block {
            kind: BlockKind::Heading { level },
            text: text.to_string(),
            source_unit: unit,
        }
    }

    fn entry(label: &str, depth: u8, start_block: Option<usize>) -> TocEntry {
        TocEntry {
            label: label.to_string(),
            depth,
            start_block,
        }
    }

    fn extraction(blocks: Vec<Block>, entries: Vec<TocEntry>, title: Option<&str>) -> Extraction {
        Extraction {
            blocks,
            toc: Toc { entries },
            biblio: Biblio {
                title: title.map(str::to_string),
                ..Default::default()
            },
            provenance: Provenance {
                adapter: "test".to_string(),
                extractor_version: 1,
                text_layer_quality: TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        }
    }

    fn ingest(corpus: &mut Corpus, intake_id: i64, ex: &Extraction) -> StructureReport {
        ingest_structure(
            corpus,
            intake_id,
            NodeType::Work,
            ex,
            &StructureParams::default(),
        )
        .expect("ingest")
    }

    /// A two-chapter book; chapter one holds a section. Heading blocks
    /// open each division and are suppressed in favour of the organizing
    /// node titles.
    fn sample() -> Extraction {
        extraction(
            vec![
                heading("Chapter One", 1, 0),
                body("Intro paragraph.", 0),
                heading("Section A", 2, 1),
                body("Section body.", 1),
                heading("Chapter Two", 1, 2),
                body("Second chapter body.", 2),
            ],
            vec![
                entry("Chapter One", 0, Some(0)),
                entry("Section A", 1, Some(2)),
                entry("Chapter Two", 0, Some(4)),
            ],
            Some("A Test Book"),
        )
    }

    #[test]
    fn builds_the_expected_tree() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 1, &sample());

        // root + 3 organizing nodes + 3 prose leaves (headings suppressed).
        assert_eq!(report.nodes_written, 7);
        assert_eq!(report.prose_leaves, 3);

        let root = corpus
            .get_node(report.book_root_id)
            .expect("get")
            .expect("root present");
        assert_eq!(root.node_type, NodeType::Work);
        assert_eq!(root.title.as_deref(), Some("A Test Book"));
        assert_eq!(root.parent_id, None);
        assert_eq!(root.depth, 0);

        // Root's children are the two chapters, in order.
        let chapters = corpus.children(report.book_root_id).expect("children");
        assert_eq!(chapters.len(), 2);
        assert!(chapters.iter().all(|c| c.node_type == NodeType::Chapter));
        assert_eq!(chapters[0].title.as_deref(), Some("Chapter One"));
        assert_eq!(chapters[1].title.as_deref(), Some("Chapter Two"));

        // Chapter one holds a direct prose leaf (ordinal 0) before the
        // section (ordinal 1).
        let ch1_children = corpus.children(chapters[0].node_id).expect("children");
        assert_eq!(ch1_children.len(), 2);
        assert_eq!(ch1_children[0].node_type, NodeType::Paragraph);
        assert_eq!(
            ch1_children[0].text_content.as_deref(),
            Some("Intro paragraph.")
        );
        assert_eq!(ch1_children[1].node_type, NodeType::Section);
        assert_eq!(ch1_children[1].title.as_deref(), Some("Section A"));
    }

    #[test]
    fn toc_intervals_nest() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 1, &sample());

        let root = corpus.get_node(report.book_root_id).unwrap().unwrap();
        // Three leaves => document-order coordinates 0..=2.
        assert_eq!(root.toc_lo, Some(0));
        assert_eq!(root.toc_hi, Some(2));

        let chapters = corpus.children(report.book_root_id).unwrap();
        // Chapter one covers its own leaf plus the section's leaf.
        assert_eq!(chapters[0].toc_lo, Some(0));
        assert_eq!(chapters[0].toc_hi, Some(1));
        // Chapter two covers only the last leaf.
        assert_eq!(chapters[1].toc_lo, Some(2));
        assert_eq!(chapters[1].toc_hi, Some(2));
    }

    #[test]
    fn prose_leaves_carry_consistent_hashes() {
        // Extra internal spaces collapse under normalization, so the
        // raw-byte hash and the normalized-text hash genuinely differ.
        let raw = "Intro   paragraph.";
        let ex = extraction(vec![body(raw, 0)], Vec::new(), None);
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 1, &ex);

        let leaf = &corpus.children(report.book_root_id).unwrap()[0];
        let norm = norm_text_sha256(raw);
        assert_eq!(leaf.norm_text_sha256.as_deref(), Some(norm.as_str()));
        // The stable anchor is the 16-hex prefix of the normalized hash.
        assert_eq!(leaf.stable_anchor.as_deref(), Some(&norm[..16]));
        // The raw-byte hash differs once normalization changes the text.
        assert!(leaf.text_sha256.is_some());
        assert_ne!(leaf.text_sha256, leaf.norm_text_sha256);
        // The display text and char count are the raw, un-normalized form.
        assert_eq!(leaf.text_content.as_deref(), Some(raw));
        assert_eq!(leaf.char_count, Some(raw.chars().count() as i64));
        assert_eq!(leaf.sentence_count, Some(1));
    }

    #[test]
    fn organizing_nodes_carry_a_subtree_signature() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 1, &sample());

        let root = corpus.get_node(report.book_root_id).unwrap().unwrap();
        assert!(root.subtree_content_sha256.is_some());
        for chapter in corpus.children(report.book_root_id).unwrap() {
            assert!(chapter.subtree_content_sha256.is_some());
            // Organizing nodes never carry body text or prose hashes.
            assert_eq!(chapter.text_content, None);
            assert_eq!(chapter.norm_text_sha256, None);
        }
    }

    #[test]
    fn front_matter_attaches_under_the_root() {
        // A paragraph precedes the first chapter's anchor.
        let ex = extraction(
            vec![
                body("Front matter.", 0),
                heading("Chapter One", 1, 1),
                body("Chapter body.", 1),
            ],
            vec![entry("Chapter One", 0, Some(1))],
            Some("Book"),
        );
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 1, &ex);

        let children = corpus.children(report.book_root_id).unwrap();
        // The front-matter leaf (ordinal 0) sits before the chapter.
        assert_eq!(children[0].node_type, NodeType::Paragraph);
        assert_eq!(children[0].text_content.as_deref(), Some("Front matter."));
        assert_eq!(children[1].node_type, NodeType::Chapter);
    }

    #[test]
    fn an_empty_toc_puts_every_leaf_under_the_root() {
        let ex = extraction(
            vec![
                body("Only paragraph one.", 0),
                body("Only paragraph two.", 0),
            ],
            Vec::new(),
            None,
        );
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 1, &ex);

        assert_eq!(report.nodes_written, 3); // root + 2 leaves
        let children = corpus.children(report.book_root_id).unwrap();
        assert_eq!(children.len(), 2);
        assert!(children.iter().all(|c| c.node_type == NodeType::Paragraph));
    }

    #[test]
    fn an_unresolved_entry_still_becomes_an_organizing_node() {
        let ex = extraction(
            vec![body("Body under no anchor.", 0)],
            vec![entry("Dangling", 0, None)],
            None,
        );
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 1, &ex);

        // The dangling entry owns no blocks; its leaf falls under the root.
        let children = corpus.children(report.book_root_id).unwrap();
        let chapter = children
            .iter()
            .find(|c| c.node_type == NodeType::Chapter)
            .expect("organizing node exists");
        assert_eq!(chapter.title.as_deref(), Some("Dangling"));
        assert!(corpus.children(chapter.node_id).unwrap().is_empty());
    }

    #[test]
    fn block_kinds_map_to_node_types() {
        let ex = extraction(
            vec![
                body("A paragraph.", 0),
                Block {
                    kind: BlockKind::Footnote,
                    text: "A footnote.".to_string(),
                    source_unit: 0,
                },
                Block {
                    kind: BlockKind::Caption,
                    text: "A caption.".to_string(),
                    source_unit: 0,
                },
                Block {
                    kind: BlockKind::Other,
                    text: "Something else.".to_string(),
                    source_unit: 0,
                },
            ],
            Vec::new(),
            None,
        );
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 1, &ex);

        let children = corpus.children(report.book_root_id).unwrap();
        let kinds: Vec<NodeType> = children.iter().map(|c| c.node_type).collect();
        assert_eq!(
            kinds,
            vec![
                NodeType::Paragraph,
                NodeType::Footnote,
                NodeType::FigureCaption,
                NodeType::Paragraph,
            ]
        );
        // The structural caption carries no content hashes.
        assert_eq!(children[2].norm_text_sha256, None);
        assert_eq!(children[2].text_content.as_deref(), Some("A caption."));
    }

    #[test]
    fn an_extraction_with_no_prose_is_rejected() {
        let ex = extraction(
            vec![Block {
                kind: BlockKind::Caption,
                text: "Lonely caption.".to_string(),
                source_unit: 0,
            }],
            Vec::new(),
            None,
        );
        let mut corpus = Corpus::open_in_memory().expect("open");
        let err = ingest_structure(
            &mut corpus,
            1,
            NodeType::Work,
            &ex,
            &StructureParams::default(),
        )
        .expect_err("must reject");
        assert!(matches!(err, IngestError::EmptyExtraction));
    }

    #[test]
    fn toc_stats_counts_entries_and_unanchored() {
        let ex = extraction(
            vec![
                heading("Chapter One", 1, 0),
                body("Body.", 0),
                heading("Chapter Two", 1, 1),
                body("Body.", 1),
            ],
            vec![
                entry("Chapter One", 0, Some(0)),
                entry("Chapter Two", 0, Some(2)),
                entry("Phantom", 0, None),
            ],
            None,
        );
        let mut corpus = Corpus::open_in_memory().expect("open");
        let stats = ingest(&mut corpus, 1, &ex).toc_stats;
        assert_eq!(stats.total_toc_entries, 3);
        assert_eq!(stats.unanchored_toc_entries, 1);
        // Three entries below the flat-TOC minimum: do not flag.
        assert!(!stats.suspicious_flat);
    }

    #[test]
    fn toc_stats_flags_suspiciously_flat_toc() {
        // Five entries all at depth 0 with a hierarchy that could have
        // expressed nesting flags suspicious_flat.
        let blocks: Vec<Block> = (0..5)
            .map(|i| heading(&format!("Title {i}"), 1, i as u32))
            .chain((0..5).map(|i| body(&format!("Body {i}."), i as u32)))
            .collect();
        let entries: Vec<TocEntry> = (0..5)
            .map(|i| entry(&format!("Title {i}"), 0, Some(i)))
            .collect();
        let ex = extraction(blocks, entries, None);
        let mut corpus = Corpus::open_in_memory().expect("open");
        let stats = ingest(&mut corpus, 1, &ex).toc_stats;
        assert!(stats.suspicious_flat);
        // Five entries with five heading blocks: not skewed.
        assert!(!stats.heading_block_skew);
    }

    #[test]
    fn toc_stats_flags_heading_block_skew() {
        // Six TOC entries but the body carries no heading blocks at all:
        // the TOC and the body disagree about structure.
        let blocks: Vec<Block> = (0..6).map(|i| body("Body.", i as u32)).collect();
        let entries: Vec<TocEntry> = (0..6)
            .map(|i| entry(&format!("Title {i}"), (i as u8) % 2, Some(i)))
            .collect();
        let ex = extraction(blocks, entries, None);
        let mut corpus = Corpus::open_in_memory().expect("open");
        let stats = ingest(&mut corpus, 1, &ex).toc_stats;
        assert!(stats.heading_block_skew);
        // Two alternating depths: not flat.
        assert!(!stats.suspicious_flat);
    }

    #[test]
    fn toc_stats_default_for_empty_toc() {
        let ex = extraction(vec![body("Body.", 0)], Vec::new(), None);
        let mut corpus = Corpus::open_in_memory().expect("open");
        let stats = ingest(&mut corpus, 1, &ex).toc_stats;
        assert_eq!(stats.total_toc_entries, 0);
        assert_eq!(stats.unanchored_toc_entries, 0);
        assert!(!stats.suspicious_flat);
        assert!(!stats.heading_block_skew);
    }

    #[test]
    fn re_ingesting_replaces_the_tree() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let first = ingest(&mut corpus, 1, &sample());

        // A second run for the same intake must not collide on the
        // partition and must not double the node count.
        let second = ingest(&mut corpus, 1, &sample());
        assert_eq!(first.book_root_id, second.book_root_id);
        assert_eq!(second.nodes_written, 7);
        assert_eq!(corpus.book_nodes(second.book_root_id).unwrap().len(), 7);
    }

    #[test]
    fn allocated_ids_stay_inside_the_partition() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 5, &sample());
        let partition = PartitionIdx::new(5);
        for node in corpus.book_nodes(report.book_root_id).unwrap() {
            assert!(partition.contains(node.node_id));
        }
    }

    #[test]
    fn plan_book_chunks_reads_prose_leaves_from_the_corpus() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 1, &sample());

        let plans =
            plan_book_chunks(&corpus, report.book_root_id, &ChunkParams::default()).expect("chunk");

        // Every chunk's text comes from a prose leaf; the sample's three
        // short leaves fit in their per-chapter groups.
        assert!(!plans.is_empty());
        let prose_ids: Vec<NodeId> = corpus
            .book_nodes(report.book_root_id)
            .unwrap()
            .into_iter()
            .filter(|n| n.node_type.is_prose_leaf())
            .map(|n| n.node_id)
            .collect();
        for plan in &plans {
            assert!(prose_ids.contains(&plan.start_node_id));
            assert!(prose_ids.contains(&plan.end_node_id));
            assert!(!plan.text.is_empty());
            assert_eq!(
                plan.norm_chunk_sha256,
                bookrack_normalize::norm_text_sha256(&plan.text)
            );
        }
    }

    #[test]
    fn planning_chunks_for_an_empty_book_is_empty() {
        // A book whose only leaf is structural (no prose) yields no chunks.
        let ex = extraction(
            vec![Block {
                kind: BlockKind::Body,
                text: "Only paragraph.".to_string(),
                source_unit: 0,
            }],
            Vec::new(),
            None,
        );
        let mut corpus = Corpus::open_in_memory().expect("open");
        let report = ingest(&mut corpus, 1, &ex);
        // Re-chunking a real book yields the same plans twice (determinism
        // across a corpus round-trip).
        let a = plan_book_chunks(&corpus, report.book_root_id, &ChunkParams::default()).unwrap();
        let b = plan_book_chunks(&corpus, report.book_root_id, &ChunkParams::default()).unwrap();
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod book_pipeline_tests {
    use super::*;
    use bookrack_core::PartitionIdx;
    use bookrack_embed::{EmbedError, Embedder, Result as EmbedResult};
    use std::future::Future;
    use std::io::Write;

    /// A fake embedder returning constant `dim`-length vectors.
    struct Fake {
        dim: usize,
    }

    impl Embedder for Fake {
        fn embed_batch(
            &self,
            texts: &[String],
        ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
            let (dim, n) = (self.dim, texts.len());
            async move { Ok(vec![vec![0.25f32; dim]; n]) }
        }
    }

    /// A fake embedder that always fails, forcing the EMBED stage to fail.
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

    /// Write a tiny plain-text book; each non-blank line becomes a block.
    fn write_sample(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("sample.txt");
        let mut file = std::fs::File::create(&path).expect("create sample");
        writeln!(
            file,
            "The first paragraph of a short sample document about birds."
        )
        .unwrap();
        writeln!(
            file,
            "A second paragraph carrying more prose to chunk and embed."
        )
        .unwrap();
        writeln!(
            file,
            "A third and final paragraph rounding out the sample text."
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn a_successful_ingest_advances_state_and_logs_ok_audit_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = write_sample(dir.path());
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let report = ingest_book(
            &file,
            &mut corpus,
            &mut catalog,
            dir.path(),
            dir.path(),
            &Fake { dim: 8 },
            &IngestParams::default(),
        )
        .await
        .expect("ingest");
        assert!(report.chunks_written > 0);
        // A bare .txt yields no biblio: the audit lands `needs_work`,
        // and the report bubbles it so the CLI can warn on stdout.
        assert_eq!(report.audit_verdict.as_deref(), Some("needs_work"));
        assert_eq!(report.audit_confidence.as_deref(), Some("low"));

        let root = report.book_root_id.get();
        let state = catalog.book_state(root).expect("state").expect("present");
        assert_eq!(state.current_stage, "embed");
        assert!(state.embedded_at.is_some());
        assert!(state.last_error.is_none());

        // The first embed stamps the index with the build parameters: the
        // configured model, the probed vector width, and the two algorithm
        // versions.
        assert_eq!(
            corpus
                .meta_get(bookrack_corpus::EMBED_MODEL_KEY)
                .expect("get"),
            Some(IngestParams::default().embed.model)
        );
        assert_eq!(
            corpus
                .meta_get(bookrack_corpus::VECTOR_DIM_KEY)
                .expect("get"),
            Some("8".to_string())
        );
        assert_eq!(
            corpus
                .meta_get(bookrack_corpus::CHUNK_VERSION_KEY)
                .expect("get"),
            Some(CHUNK_VERSION.to_string())
        );
        assert_eq!(
            corpus
                .meta_get(bookrack_corpus::NORMALIZE_VERSION_KEY)
                .expect("get"),
            Some(bookrack_normalize::NORMALIZE_VERSION.to_string())
        );

        // Every rooted stage logged a row, with the non-blocking metadata
        // sub-step sitting between structure and chunk; the embed row
        // carries its batching metrics.
        let rows = catalog.pipeline_audit_for_book(root).expect("audit");
        let stages: Vec<&str> = rows.iter().map(|r| r.stage.as_str()).collect();
        assert_eq!(stages, ["structure", "metadata", "chunk", "embed"]);
        let metadata_row = rows
            .iter()
            .find(|r| r.stage == "metadata")
            .expect("metadata row");
        // A bare .txt yields no biblio, so the audit's verdict is
        // `needs_work` and the metadata row outcome reads `partial`.
        assert_eq!(metadata_row.outcome, "partial");
        assert!(
            rows.iter()
                .filter(|r| r.stage != "metadata")
                .all(|r| r.outcome == "ok")
        );
        let embed = rows.iter().find(|r| r.stage == "embed").expect("embed row");
        let metric = embed.metric_summary.as_deref().unwrap_or_default();
        assert!(metric.contains("\"batches\""), "metric: {metric}");

        // The advisory node_reviews row is `pending` for every fresh
        // ingest — the audit never grants approval. The bare-text book
        // still embeds: the audit never gates EMBED. The row-level
        // confidence rolled back into node_publication_attrs records the
        // audit's gap; the notes column carries the verdict token.
        let review = catalog
            .review(report.intake_id, "book")
            .expect("review")
            .expect("present");
        assert_eq!(review.status, bookrack_catalog::STATUS_PENDING);
        assert!(
            review
                .notes
                .as_deref()
                .unwrap_or_default()
                .contains("verdict=needs_work"),
            "notes must carry the audit verdict for forensics: {:?}",
            review.notes
        );
        let intake = catalog
            .intake_by_id(report.intake_id)
            .expect("intake")
            .expect("present");
        assert_eq!(intake.status, bookrack_catalog::IntakeStatus::Embedded);
        let attrs = catalog
            .publication_attrs(report.intake_id, "book")
            .expect("attrs")
            .expect("present");
        assert_eq!(attrs.confidence.as_deref(), Some("low"));
        // Verdict is stamped alongside confidence so `metadata show`
        // and `metadata list` agree on the historical audit outcome.
        assert_eq!(attrs.audit_verdict.as_deref(), Some("needs_work"));
    }

    #[tokio::test]
    async fn re_ingest_of_an_up_to_date_source_is_a_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = write_sample(dir.path());
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let first = ingest_book(
            &file,
            &mut corpus,
            &mut catalog,
            dir.path(),
            dir.path(),
            &Fake { dim: 8 },
            &IngestParams::default(),
        )
        .await
        .expect("first ingest");
        assert!(first.chunks_written > 0);
        assert!(!first.no_op);
        assert!(!first.forced);

        let baseline = catalog
            .pipeline_audit_for_book(first.book_root_id.get())
            .expect("audit")
            .len();

        // Second ingest of the same file: the source SHA is on file,
        // status is Embedded, extractor_version and embed_model both
        // match. The pipeline must short-circuit.
        let second = ingest_book(
            &file,
            &mut corpus,
            &mut catalog,
            dir.path(),
            dir.path(),
            &Fake { dim: 8 },
            &IngestParams::default(),
        )
        .await
        .expect("second ingest");
        assert!(second.no_op, "second ingest should be a no-op");
        assert_eq!(second.intake_id, first.intake_id);
        assert_eq!(second.chunks_written, 0);
        assert_eq!(second.nodes_written, 0);
        assert!(second.already_registered);
        assert!(!second.forced);
        // A noop run still surfaces the stored audit outcome so the
        // operator sees the historical verdict on a re-ingest.
        assert_eq!(second.audit_verdict, first.audit_verdict);
        assert_eq!(second.audit_confidence, first.audit_confidence);

        // The pipeline audit must not have grown: no new stage rows.
        let after = catalog
            .pipeline_audit_for_book(first.book_root_id.get())
            .expect("audit")
            .len();
        assert_eq!(after, baseline, "no new audit rows on a noop re-ingest");
    }

    #[tokio::test]
    async fn force_re_ingest_runs_the_full_pipeline_even_when_up_to_date() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = write_sample(dir.path());
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let first = ingest_book(
            &file,
            &mut corpus,
            &mut catalog,
            dir.path(),
            dir.path(),
            &Fake { dim: 8 },
            &IngestParams::default(),
        )
        .await
        .expect("first ingest");
        let baseline = catalog
            .pipeline_audit_for_book(first.book_root_id.get())
            .expect("audit")
            .len();

        let forced = ingest_book(
            &file,
            &mut corpus,
            &mut catalog,
            dir.path(),
            dir.path(),
            &Fake { dim: 8 },
            &IngestParams {
                force: true,
                ..Default::default()
            },
        )
        .await
        .expect("forced ingest");
        assert!(!forced.no_op);
        assert!(forced.forced);
        assert!(forced.already_registered);
        assert!(forced.chunks_written > 0);

        // The audit trail has grown: forced ingest re-ran every stage.
        let after = catalog
            .pipeline_audit_for_book(first.book_root_id.get())
            .expect("audit")
            .len();
        assert!(after > baseline, "forced ingest must add new audit rows");
    }

    #[tokio::test]
    async fn ingest_writes_envelope_and_records_stored_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let books_dir = dir.path().join("books");
        let file = write_sample(dir.path());
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let report = ingest_book(
            &file,
            &mut corpus,
            &mut catalog,
            dir.path(),
            &books_dir,
            &Fake { dim: 8 },
            &IngestParams::default(),
        )
        .await
        .expect("ingest");

        let intake = catalog
            .intake_by_id(report.intake_id)
            .expect("intake")
            .expect("present");
        let stored_path = intake
            .stored_path
            .clone()
            .expect("stored_path must be recorded after a successful ingest");
        let envelope_path = std::path::PathBuf::from(&stored_path);
        assert_eq!(
            envelope_path.file_name().and_then(|n| n.to_str()),
            Some(envelope_filename(report.intake_id).as_str())
        );

        let envelope = read_envelope(&envelope_path).expect("read envelope");
        assert_eq!(envelope.schema_version, ENVELOPE_SCHEMA_VERSION);
        assert_eq!(envelope.intake_id, report.intake_id);
        assert!(!envelope.source_sha256.is_empty());
        assert!(!envelope.extraction.blocks.is_empty());
    }

    #[tokio::test]
    async fn envelope_write_failure_does_not_abort_ingest() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A regular file at the books_dir path: write_envelope's
        // create_dir_all then fails, exercising the warn-on-fail path.
        let books_dir = dir.path().join("books-as-file");
        std::fs::write(&books_dir, b"not a directory").expect("write");
        let file = write_sample(dir.path());
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let report = ingest_book(
            &file,
            &mut corpus,
            &mut catalog,
            dir.path(),
            &books_dir,
            &Fake { dim: 8 },
            &IngestParams::default(),
        )
        .await
        .expect("ingest must succeed even when envelope write fails");
        assert!(report.chunks_written > 0);

        let intake = catalog
            .intake_by_id(report.intake_id)
            .expect("intake")
            .expect("present");
        assert!(
            intake.stored_path.is_none(),
            "stored_path must stay null when the envelope write fails"
        );
    }

    #[tokio::test]
    async fn a_complete_biblio_still_lands_as_pending_with_a_clean_audit_verdict() {
        // Drive `run_metadata_substep` directly on a synthetic extraction
        // whose biblio carries the required fields: the node_reviews row
        // stays `pending` (the pipeline never grants review approval),
        // but the audit's own verdict in the notes column reads `clean`
        // and the pipeline-audit row's outcome reads `ok`.
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = bookrack_extract::Extraction {
            blocks: vec![bookrack_extract::Block {
                kind: bookrack_extract::BlockKind::Body,
                text: "A short English body sample for the audit.".to_string(),
                source_unit: 0,
            }],
            toc: bookrack_extract::Toc {
                entries: Vec::new(),
            },
            biblio: bookrack_extract::Biblio {
                title: Some("A Complete Book".to_string()),
                language: Some("en".to_string()),
                publisher: Some("Oxford University Press".to_string()),
                year: Some(2010),
                ..Default::default()
            },
            provenance: bookrack_extract::Provenance {
                adapter: "epub".to_string(),
                extractor_version: 1,
                text_layer_quality: bookrack_extract::TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        };
        let intake = catalog
            .register_intake(&bookrack_catalog::NewIntake::new("dummy-sha".to_string()))
            .expect("register");
        let intake_id = intake.intake().intake_id;
        super::run_metadata_substep(
            &catalog,
            intake_id,
            42,
            &extraction,
            &TocStats::default(),
            Some("a-complete-book"),
            None,
            &bookrack_metadata::AuditRules::empty(),
            &bookrack_metadata::AuditProfile::default(),
            "run-1",
            "dummy-sha",
        );
        let review = catalog
            .review(intake_id, "book")
            .expect("review")
            .expect("present");
        assert_eq!(review.status, bookrack_catalog::STATUS_PENDING);
        let attrs = catalog
            .publication_attrs(intake_id, "book")
            .expect("attrs")
            .expect("present");
        assert_eq!(attrs.title.as_deref(), Some("A Complete Book"));
        assert_eq!(attrs.year.as_deref(), Some("2010"));
        assert_eq!(attrs.source.as_deref(), Some("extracted"));
        assert_eq!(attrs.source_format.as_deref(), Some("epub"));
        // Required + should-fill fields all Strong → confidence high.
        assert_eq!(attrs.confidence.as_deref(), Some("high"));
        assert_eq!(attrs.audit_verdict.as_deref(), Some("clean"));
    }

    #[tokio::test]
    async fn trust_source_profile_writes_pending_review_and_skips_audit() {
        // Same fixture as the default-profile case above, but driven by
        // the trust-source profile: the audit substep is short-circuited.
        // The review row still lands at `pending`, the reviewed_by carries
        // the profile name, and `node_publication_attrs.confidence` is
        // never written because no MetadataReport is produced.
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = bookrack_extract::Extraction {
            blocks: vec![bookrack_extract::Block {
                kind: bookrack_extract::BlockKind::Body,
                text: "A short English body sample for the audit.".to_string(),
                source_unit: 0,
            }],
            toc: bookrack_extract::Toc {
                entries: Vec::new(),
            },
            biblio: bookrack_extract::Biblio {
                title: Some("A Complete Book".to_string()),
                language: Some("en".to_string()),
                publisher: Some("Oxford University Press".to_string()),
                year: Some(2010),
                ..Default::default()
            },
            provenance: bookrack_extract::Provenance {
                adapter: "epub".to_string(),
                extractor_version: 1,
                text_layer_quality: bookrack_extract::TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        };
        let intake = catalog
            .register_intake(&bookrack_catalog::NewIntake::new("dummy-sha".to_string()))
            .expect("register");
        let intake_id = intake.intake().intake_id;
        super::run_metadata_substep(
            &catalog,
            intake_id,
            42,
            &extraction,
            &TocStats::default(),
            Some("a-complete-book"),
            None,
            &bookrack_metadata::AuditRules::empty(),
            &bookrack_metadata::AuditProfile::trust_source(),
            "run-1",
            "dummy-sha",
        );
        let review = catalog
            .review(intake_id, "book")
            .expect("review")
            .expect("present");
        assert_eq!(review.status, bookrack_catalog::STATUS_PENDING);
        assert_eq!(review.reviewed_by, "bookrack-ingest:trust-source");
        assert!(
            review
                .notes
                .as_deref()
                .unwrap_or_default()
                .contains("audit skipped"),
            "trust-source review row must note the skip in the notes column: {:?}",
            review.notes,
        );
        let attrs = catalog
            .publication_attrs(intake_id, "book")
            .expect("attrs")
            .expect("present");
        // Base attrs are still seeded; the audit-derived confidence
        // and verdict are not.
        assert_eq!(attrs.title.as_deref(), Some("A Complete Book"));
        assert!(attrs.confidence.is_none());
        assert!(attrs.audit_verdict.is_none());
    }

    #[tokio::test]
    async fn a_bare_book_takes_publisher_and_year_from_the_filename() {
        // The extractor returns an empty biblio but the input filename
        // matches the `Author - Title (Year, Publisher)` template, so
        // base attrs fill in from the filename parse with
        // `source = "filename"`.
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = bookrack_extract::Extraction {
            blocks: vec![bookrack_extract::Block {
                kind: bookrack_extract::BlockKind::Body,
                text: "A short English body sample for the audit.".to_string(),
                source_unit: 0,
            }],
            toc: bookrack_extract::Toc {
                entries: Vec::new(),
            },
            biblio: bookrack_extract::Biblio::default(),
            provenance: bookrack_extract::Provenance {
                adapter: "txt".to_string(),
                extractor_version: 1,
                text_layer_quality: bookrack_extract::TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        };
        let intake = catalog
            .register_intake(&bookrack_catalog::NewIntake::new("dummy-sha".to_string()))
            .expect("register");
        let intake_id = intake.intake().intake_id;
        let stem = "Alice Author - A Sample Title (2003, Sample Press)";
        let filename_biblio = bookrack_metadata::parse_filename(
            stem,
            &bookrack_metadata::AuditProfile::default().filename_parser,
        );
        super::run_metadata_substep(
            &catalog,
            intake_id,
            42,
            &extraction,
            &TocStats::default(),
            Some(stem),
            Some(&filename_biblio),
            &bookrack_metadata::AuditRules::empty(),
            &bookrack_metadata::AuditProfile::default(),
            "run-1",
            "dummy-sha",
        );
        let attrs = catalog
            .publication_attrs(intake_id, "book")
            .expect("attrs")
            .expect("present");
        assert_eq!(attrs.title.as_deref(), Some("A Sample Title"));
        assert_eq!(attrs.publisher.as_deref(), Some("Sample Press"));
        assert_eq!(attrs.year.as_deref(), Some("2003"));
        assert_eq!(attrs.source.as_deref(), Some("filename"));
        assert_eq!(attrs.source_format.as_deref(), Some("txt"));
    }

    #[tokio::test]
    async fn extracted_biblio_wins_over_filename_per_field() {
        // The extractor provides a title; the filename also supplies a
        // title and a publisher the extractor lacks. The extracted
        // title wins; the publisher gap fills from the filename; the
        // row still reads `source = "extracted"` because the adapter
        // contributed at least one field.
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = bookrack_extract::Extraction {
            blocks: vec![bookrack_extract::Block {
                kind: bookrack_extract::BlockKind::Body,
                text: "A short English body sample for the audit.".to_string(),
                source_unit: 0,
            }],
            toc: bookrack_extract::Toc {
                entries: Vec::new(),
            },
            biblio: bookrack_extract::Biblio {
                title: Some("Extracted Title".to_string()),
                language: Some("en".to_string()),
                ..Default::default()
            },
            provenance: bookrack_extract::Provenance {
                adapter: "pdf".to_string(),
                extractor_version: 1,
                text_layer_quality: bookrack_extract::TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        };
        let intake = catalog
            .register_intake(&bookrack_catalog::NewIntake::new("dummy-sha".to_string()))
            .expect("register");
        let intake_id = intake.intake().intake_id;
        let stem = "Alice Author - Filename Title (2003, Sample Press)";
        let filename_biblio = bookrack_metadata::parse_filename(
            stem,
            &bookrack_metadata::AuditProfile::default().filename_parser,
        );
        super::run_metadata_substep(
            &catalog,
            intake_id,
            42,
            &extraction,
            &TocStats::default(),
            Some(stem),
            Some(&filename_biblio),
            &bookrack_metadata::AuditRules::empty(),
            &bookrack_metadata::AuditProfile::default(),
            "run-1",
            "dummy-sha",
        );
        let attrs = catalog
            .publication_attrs(intake_id, "book")
            .expect("attrs")
            .expect("present");
        assert_eq!(attrs.title.as_deref(), Some("Extracted Title"));
        assert_eq!(attrs.publisher.as_deref(), Some("Sample Press"));
        assert_eq!(attrs.year.as_deref(), Some("2003"));
        assert_eq!(attrs.source.as_deref(), Some("extracted"));
    }

    #[tokio::test]
    async fn an_invalid_extracted_isbn_yields_to_a_filename_isbn() {
        // The adapter reads a ten-digit purely numeric value (the shape
        // some EPUB toolchains stamp into `<dc:identifier>` when no real
        // ISBN exists). The filename carries a real, checksum-valid
        // ISBN-13. The base attrs drop the invalid extraction and let
        // the filename ISBN take over.
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = bookrack_extract::Extraction {
            blocks: vec![bookrack_extract::Block {
                kind: bookrack_extract::BlockKind::Body,
                text: "A short English body sample for the audit.".to_string(),
                source_unit: 0,
            }],
            toc: bookrack_extract::Toc {
                entries: Vec::new(),
            },
            biblio: bookrack_extract::Biblio {
                title: Some("Extracted Title".to_string()),
                isbn: Some("1742443234".to_string()),
                ..Default::default()
            },
            provenance: bookrack_extract::Provenance {
                adapter: "epub".to_string(),
                extractor_version: 1,
                text_layer_quality: bookrack_extract::TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        };
        let intake = catalog
            .register_intake(&bookrack_catalog::NewIntake::new("dummy-sha".to_string()))
            .expect("register");
        let intake_id = intake.intake().intake_id;
        let stem = "Alice Author - A Sample Title -- isbn13 9780306406157";
        let filename_biblio = bookrack_metadata::parse_filename(
            stem,
            &bookrack_metadata::AuditProfile::default().filename_parser,
        );
        super::run_metadata_substep(
            &catalog,
            intake_id,
            42,
            &extraction,
            &TocStats::default(),
            Some(stem),
            Some(&filename_biblio),
            &bookrack_metadata::AuditRules::empty(),
            &bookrack_metadata::AuditProfile::default(),
            "run-1",
            "dummy-sha",
        );
        let attrs = catalog
            .publication_attrs(intake_id, "book")
            .expect("attrs")
            .expect("present");
        assert_eq!(attrs.isbn.as_deref(), Some("9780306406157"));
        assert_eq!(attrs.source.as_deref(), Some("extracted"));
    }

    #[tokio::test]
    async fn an_extracted_timestamp_year_yields_to_a_disagreeing_filename_year() {
        // The adapter parses a `<dc:date>` of `2019-04-01T00:00:00Z` —
        // timestamp shape, often the EPUB build date rather than the
        // publication year. The filename carries `(2006, ...)`. Since
        // the two disagree and the extraction's raw date is in timestamp
        // shape, the filename year wins.
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = bookrack_extract::Extraction {
            blocks: vec![bookrack_extract::Block {
                kind: bookrack_extract::BlockKind::Body,
                text: "A short English body sample for the audit.".to_string(),
                source_unit: 0,
            }],
            toc: bookrack_extract::Toc {
                entries: Vec::new(),
            },
            biblio: bookrack_extract::Biblio {
                title: Some("Extracted Title".to_string()),
                year: Some(2019),
                year_raw: Some("2019-04-01T00:00:00Z".to_string()),
                ..Default::default()
            },
            provenance: bookrack_extract::Provenance {
                adapter: "epub".to_string(),
                extractor_version: 1,
                text_layer_quality: bookrack_extract::TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        };
        let intake = catalog
            .register_intake(&bookrack_catalog::NewIntake::new("dummy-sha".to_string()))
            .expect("register");
        let intake_id = intake.intake().intake_id;
        let stem = "Alice Author - A Sample Title (2006, Sample Press)";
        let filename_biblio = bookrack_metadata::parse_filename(
            stem,
            &bookrack_metadata::AuditProfile::default().filename_parser,
        );
        super::run_metadata_substep(
            &catalog,
            intake_id,
            42,
            &extraction,
            &TocStats::default(),
            Some(stem),
            Some(&filename_biblio),
            &bookrack_metadata::AuditRules::empty(),
            &bookrack_metadata::AuditProfile::default(),
            "run-1",
            "dummy-sha",
        );
        let attrs = catalog
            .publication_attrs(intake_id, "book")
            .expect("attrs")
            .expect("present");
        assert_eq!(attrs.year.as_deref(), Some("2006"));
        assert_eq!(attrs.source.as_deref(), Some("extracted"));
    }

    #[tokio::test]
    async fn a_timestamp_year_matching_the_filename_year_is_kept() {
        // Same timestamp shape, but the filename agrees on `2019`. The
        // extracted year is retained — the filter fires only on a
        // disagreement.
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let extraction = bookrack_extract::Extraction {
            blocks: vec![bookrack_extract::Block {
                kind: bookrack_extract::BlockKind::Body,
                text: "A short English body sample for the audit.".to_string(),
                source_unit: 0,
            }],
            toc: bookrack_extract::Toc {
                entries: Vec::new(),
            },
            biblio: bookrack_extract::Biblio {
                title: Some("Extracted Title".to_string()),
                year: Some(2019),
                year_raw: Some("2019-04-01T00:00:00Z".to_string()),
                ..Default::default()
            },
            provenance: bookrack_extract::Provenance {
                adapter: "epub".to_string(),
                extractor_version: 1,
                text_layer_quality: bookrack_extract::TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        };
        let intake = catalog
            .register_intake(&bookrack_catalog::NewIntake::new("dummy-sha".to_string()))
            .expect("register");
        let intake_id = intake.intake().intake_id;
        let stem = "Alice Author - A Sample Title (2019, Sample Press)";
        let filename_biblio = bookrack_metadata::parse_filename(
            stem,
            &bookrack_metadata::AuditProfile::default().filename_parser,
        );
        super::run_metadata_substep(
            &catalog,
            intake_id,
            42,
            &extraction,
            &TocStats::default(),
            Some(stem),
            Some(&filename_biblio),
            &bookrack_metadata::AuditRules::empty(),
            &bookrack_metadata::AuditProfile::default(),
            "run-1",
            "dummy-sha",
        );
        let attrs = catalog
            .publication_attrs(intake_id, "book")
            .expect("attrs")
            .expect("present");
        assert_eq!(attrs.year.as_deref(), Some("2019"));
    }

    #[tokio::test]
    async fn hold_for_metadata_parks_a_bare_book_at_the_metadata_stage() {
        // A bare .txt yields no biblio; with the hold gate on, ingest
        // stops at metadata rather than chunking and embedding.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = write_sample(dir.path());
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let params = IngestParams {
            hold_for_metadata: true,
            ..Default::default()
        };
        let report = ingest_book(
            &file,
            &mut corpus,
            &mut catalog,
            dir.path(),
            dir.path(),
            &Fake { dim: 8 },
            &params,
        )
        .await
        .expect("ingest");
        // The hold tripped: no chunks were embedded.
        assert_eq!(report.chunks_written, 0);

        let root = report.book_root_id.get();
        let state = catalog.book_state(root).expect("state").expect("present");
        assert_eq!(state.current_stage, "metadata");
        let intake = catalog
            .intake_by_id(report.intake_id)
            .expect("intake")
            .expect("present");
        assert_ne!(intake.status, IntakeStatus::Embedded);

        // Now satisfy the gate (clean title + language) and resume.
        let mut attrs = bookrack_catalog::NewPublicationAttrs::new(report.intake_id, "book");
        attrs.title = Some("A Title".to_string());
        attrs.language = Some("en".to_string());
        catalog.upsert_publication_attrs(&attrs).expect("attrs");
        let resume = resume_from_chunk(
            &mut corpus,
            &mut catalog,
            dir.path(),
            &Fake { dim: 8 },
            &params,
            report.intake_id,
            report.book_root_id,
            "advance-test",
            "test-sha",
            state.parsed_at.as_deref().unwrap_or("now"),
        )
        .await
        .expect("resume");
        assert!(resume.chunks_written > 0);

        // After resume the book is fully embedded.
        let intake = catalog
            .intake_by_id(report.intake_id)
            .expect("intake")
            .expect("present");
        assert_eq!(intake.status, IntakeStatus::Embedded);
        let state = catalog.book_state(root).expect("state").expect("present");
        assert_eq!(state.current_stage, "embed");
    }

    #[tokio::test]
    async fn hold_off_by_default_keeps_advisory_semantics() {
        // Without the hold flag, a bare book still embeds even though
        // its audit verdict is needs_work.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = write_sample(dir.path());
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let report = ingest_book(
            &file,
            &mut corpus,
            &mut catalog,
            dir.path(),
            dir.path(),
            &Fake { dim: 8 },
            &IngestParams::default(),
        )
        .await
        .expect("ingest");
        assert!(report.chunks_written > 0);
        let intake = catalog
            .intake_by_id(report.intake_id)
            .expect("intake")
            .expect("present");
        assert_eq!(intake.status, IntakeStatus::Embedded);
    }

    #[tokio::test]
    async fn a_failed_embed_records_fail_outcome_and_last_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = write_sample(dir.path());
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let mut catalog = Catalog::open_in_memory().expect("catalog");

        let err = ingest_book(
            &file,
            &mut corpus,
            &mut catalog,
            dir.path(),
            dir.path(),
            &Offline,
            &IngestParams::default(),
        )
        .await
        .expect_err("embed must fail");
        assert!(matches!(err, IngestError::Embed(_)));

        // STRUCTURE allocated the root for the first intake, so book_state
        // exists and carries the failure; no vectors were written.
        let root = PartitionIdx::new(1).root().get();
        let state = catalog.book_state(root).expect("state").expect("present");
        assert_eq!(state.current_stage, "embed");
        assert!(state.embedded_at.is_none());
        assert!(state.last_error.is_some());

        let rows = catalog.pipeline_audit_for_book(root).expect("audit");
        let embed = rows.iter().find(|r| r.stage == "embed").expect("embed row");
        assert_eq!(embed.outcome, "fail");
        assert!(embed.error_message.is_some());
    }

    /// A synthetic `Extraction` parameterised on biblio fields so the
    /// `build_base_attrs` tests below can vary them one at a time.
    fn extraction_with_biblio(biblio: bookrack_extract::Biblio) -> bookrack_extract::Extraction {
        bookrack_extract::Extraction {
            blocks: Vec::new(),
            toc: bookrack_extract::Toc {
                entries: Vec::new(),
            },
            biblio,
            provenance: bookrack_extract::Provenance {
                adapter: "epub".to_string(),
                extractor_version: 1,
                text_layer_quality: bookrack_extract::TextLayerQuality::BornDigital,
                skipped_units: Vec::new(),
                derived_from_sha256: None,
                partial_pages: None,
            },
        }
    }

    #[test]
    fn build_base_attrs_emits_no_actions_on_a_clean_extraction() {
        let extraction = extraction_with_biblio(bookrack_extract::Biblio {
            title: Some("Extracted Title".to_string()),
            publisher: Some("Extracted Press".to_string()),
            year: Some(2010),
            year_raw: Some("2010".to_string()),
            ..Default::default()
        });
        let profile = bookrack_metadata::AuditProfile::default();
        let outcome = super::build_base_attrs(1, &extraction, None, &profile);
        assert!(
            outcome.actions.is_empty(),
            "expected no actions, got {:?}",
            outcome.actions
        );
        assert_eq!(outcome.attrs.source.as_deref(), Some("extracted"));
    }

    #[test]
    fn build_base_attrs_records_a_drop_invalid_isbn_action() {
        let extraction = extraction_with_biblio(bookrack_extract::Biblio {
            title: Some("T".to_string()),
            // 10 digits that fail the ISBN-10 checksum (sum = 211, 211 % 11 = 2).
            isbn: Some("1234567891".to_string()),
            ..Default::default()
        });
        let profile = bookrack_metadata::AuditProfile::default();
        let outcome = super::build_base_attrs(1, &extraction, None, &profile);
        assert!(outcome.attrs.isbn.is_none());
        let tokens: Vec<String> = outcome.actions.iter().map(|a| a.token()).collect();
        assert!(
            tokens.contains(&"drop_invalid_isbn".to_string()),
            "expected drop_invalid_isbn in {tokens:?}"
        );
    }

    #[test]
    fn drop_invalid_isbn_off_keeps_extracted_isbn() {
        // With `publisher.drop_10digit_isbn_to_filename = false` the
        // checksum-failing extracted ISBN survives, so a filename ISBN
        // cannot fill the slot.
        let extraction = extraction_with_biblio(bookrack_extract::Biblio {
            title: Some("T".to_string()),
            isbn: Some("1234567891".to_string()),
            ..Default::default()
        });
        let mut profile = bookrack_metadata::AuditProfile::default();
        profile.publisher.drop_10digit_isbn_to_filename = false;
        let outcome = super::build_base_attrs(1, &extraction, None, &profile);
        assert_eq!(outcome.attrs.isbn.as_deref(), Some("1234567891"));
        let tokens: Vec<String> = outcome.actions.iter().map(|a| a.token()).collect();
        assert!(
            !tokens.contains(&"drop_invalid_isbn".to_string()),
            "expected no drop_invalid_isbn in {tokens:?}"
        );
    }

    #[test]
    fn build_base_attrs_records_a_drop_stale_year_action() {
        let extraction = extraction_with_biblio(bookrack_extract::Biblio {
            title: Some("T".to_string()),
            year: Some(2019),
            year_raw: Some("2019-04-01T00:00:00Z".to_string()),
            ..Default::default()
        });
        let profile = bookrack_metadata::AuditProfile::default();
        let filename = bookrack_metadata::parse_filename(
            "Alice Author - A Sample Title (2006, Sample Press)",
            &profile.filename_parser,
        );
        let outcome = super::build_base_attrs(1, &extraction, Some(&filename), &profile);
        // The stale 2019 was dropped, then the filename year filled in.
        assert_eq!(outcome.attrs.year.as_deref(), Some("2006"));
        let tokens: Vec<String> = outcome.actions.iter().map(|a| a.token()).collect();
        assert!(
            tokens.contains(&"drop_stale_year".to_string()),
            "expected drop_stale_year in {tokens:?}"
        );
        assert!(
            tokens.contains(&"filename_fallback:year".to_string()),
            "expected filename_fallback:year in {tokens:?}"
        );
    }

    #[test]
    fn cross_field_year_override_off_keeps_timestamp_year() {
        // With `year.cross_field_filename_override = false` the
        // timestamp-shape extracted year survives even when the filename
        // disagrees.
        let extraction = extraction_with_biblio(bookrack_extract::Biblio {
            title: Some("T".to_string()),
            year: Some(2019),
            year_raw: Some("2019-04-01T00:00:00Z".to_string()),
            ..Default::default()
        });
        let mut profile = bookrack_metadata::AuditProfile::default();
        profile.year.cross_field_filename_override = false;
        let filename = bookrack_metadata::parse_filename(
            "Alice Author - A Sample Title (2006, Sample Press)",
            &profile.filename_parser,
        );
        let outcome = super::build_base_attrs(1, &extraction, Some(&filename), &profile);
        assert_eq!(outcome.attrs.year.as_deref(), Some("2019"));
        let tokens: Vec<String> = outcome.actions.iter().map(|a| a.token()).collect();
        assert!(
            !tokens.contains(&"drop_stale_year".to_string()),
            "expected no drop_stale_year in {tokens:?}"
        );
    }

    #[test]
    fn build_base_attrs_records_filename_fallback_actions_per_field() {
        // The adapter found nothing; the filename parser fills five fields.
        // Every fill must surface as its own action so a JSONL consumer
        // can attribute each value back to a layer.
        let extraction = extraction_with_biblio(bookrack_extract::Biblio::default());
        let profile = bookrack_metadata::AuditProfile::default();
        let filename = bookrack_metadata::parse_filename(
            "Alice Author - A Sample Title (2006, Sample Press)",
            &profile.filename_parser,
        );
        let outcome = super::build_base_attrs(1, &extraction, Some(&filename), &profile);
        let tokens: Vec<String> = outcome.actions.iter().map(|a| a.token()).collect();
        for expected in [
            "filename_fallback:title",
            "filename_fallback:publisher",
            "filename_fallback:year",
        ] {
            assert!(
                tokens.contains(&expected.to_string()),
                "expected {expected} in {tokens:?}"
            );
        }
        assert_eq!(outcome.attrs.source.as_deref(), Some("filename"));
    }

    fn intake_id(catalog: &mut Catalog, sha: &str) -> i64 {
        catalog
            .register_intake(&bookrack_catalog::NewIntake::new(sha.to_string()))
            .expect("register")
            .intake()
            .intake_id
    }

    #[test]
    fn write_contributors_persists_biblio_contributors_with_extracted_origin() {
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let intake = intake_id(&mut catalog, "biblio-sha");
        let biblio = bookrack_extract::Biblio {
            contributors: vec![
                bookrack_extract::Contributor {
                    name: "Alice Author".to_string(),
                    role: bookrack_extract::ContributorRole::Author,
                },
                bookrack_extract::Contributor {
                    name: "Bob Author".to_string(),
                    role: bookrack_extract::ContributorRole::Author,
                },
                bookrack_extract::Contributor {
                    name: "Carol Translator".to_string(),
                    role: bookrack_extract::ContributorRole::Translator,
                },
            ],
            ..Default::default()
        };
        super::write_contributors(&catalog, intake, &biblio, None);
        let rows = catalog
            .contributors_for_address(intake, BOOK_SCOPE)
            .expect("read");
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.origin == "extracted"));
        assert!(
            rows.iter()
                .any(|row| row.role == "author" && row.ordinal == 0 && row.name == "Alice Author"),
        );
        assert!(
            rows.iter()
                .any(|row| row.role == "author" && row.ordinal == 1 && row.name == "Bob Author"),
        );
        assert!(rows.iter().any(|row| row.role == "translator"
            && row.ordinal == 2
            && row.name == "Carol Translator"));
    }

    #[test]
    fn write_contributors_persists_filename_author_with_distinct_origin() {
        // The biblio is empty, so only the filename author lands. The
        // entry is tagged `extracted-filename` to distinguish it from
        // adapter-extracted contributors.
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let intake = intake_id(&mut catalog, "filename-sha");
        let profile = bookrack_metadata::AuditProfile::default();
        let filename = bookrack_metadata::parse_filename(
            "Alice Author - A Sample Title (2006, Sample Press)",
            &profile.filename_parser,
        );
        super::write_contributors(
            &catalog,
            intake,
            &bookrack_extract::Biblio::default(),
            Some(&filename),
        );
        let rows = catalog
            .contributors_for_address(intake, BOOK_SCOPE)
            .expect("read");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.role, "author");
        assert_eq!(row.ordinal, 0);
        assert_eq!(row.origin, "extracted-filename");
        assert_eq!(row.name, "Alice Author");
    }

    #[test]
    fn write_contributors_keeps_both_sources_side_by_side() {
        // Same role and ordinal across two origins: the UNIQUE constraint
        // includes origin so they coexist. A blank filename author is
        // skipped without disturbing the extracted rows.
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let intake = intake_id(&mut catalog, "dual-source-sha");
        let biblio = bookrack_extract::Biblio {
            contributors: vec![bookrack_extract::Contributor {
                name: " Murakami Haruki ".to_string(),
                role: bookrack_extract::ContributorRole::Author,
            }],
            ..Default::default()
        };
        let filename = bookrack_metadata::FilenameBiblio {
            author: Some("M. Haruki".to_string()),
            ..Default::default()
        };
        super::write_contributors(&catalog, intake, &biblio, Some(&filename));

        let blank_intake = intake_id(&mut catalog, "blank-author-sha");
        let blank_filename = bookrack_metadata::FilenameBiblio {
            author: Some("   ".to_string()),
            ..Default::default()
        };
        super::write_contributors(
            &catalog,
            blank_intake,
            &bookrack_extract::Biblio::default(),
            Some(&blank_filename),
        );

        let rows = catalog
            .contributors_for_address(intake, BOOK_SCOPE)
            .expect("read");
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().any(|row| row.origin == "extracted"
                && row.ordinal == 0
                && row.name == "Murakami Haruki"),
            "expected trimmed extracted author in {rows:?}",
        );
        assert!(
            rows.iter().any(|row| row.origin == "extracted-filename"
                && row.ordinal == 0
                && row.name == "M. Haruki"),
            "expected filename-derived author in {rows:?}",
        );
        assert!(
            catalog
                .contributors_for_address(blank_intake, BOOK_SCOPE)
                .expect("read")
                .is_empty(),
        );
    }

    #[test]
    fn write_contributors_refresh_replaces_pipeline_rows_without_duplicates() {
        // Re-running write_contributors for the same intake must replace
        // the extracted set rather than collide on the UNIQUE natural
        // key. A user-curated row sharing role and ordinal survives the
        // refresh because its origin is preserved.
        let mut catalog = Catalog::open_in_memory().expect("catalog");
        let intake = intake_id(&mut catalog, "refresh-sha");
        catalog
            .add_contributor(&bookrack_catalog::NewContributor::new(
                intake,
                BOOK_SCOPE,
                "editor",
                0,
                "user",
                "Hand Curated Editor",
            ))
            .expect("seed user row");

        let biblio = bookrack_extract::Biblio {
            contributors: vec![bookrack_extract::Contributor {
                name: "First Author".to_string(),
                role: bookrack_extract::ContributorRole::Author,
            }],
            ..Default::default()
        };
        let filename = bookrack_metadata::FilenameBiblio {
            author: Some("F. Author".to_string()),
            ..Default::default()
        };
        super::write_contributors(&catalog, intake, &biblio, Some(&filename));
        super::write_contributors(&catalog, intake, &biblio, Some(&filename));

        let rows = catalog
            .contributors_for_address(intake, BOOK_SCOPE)
            .expect("read");
        assert_eq!(
            rows.len(),
            3,
            "one extracted + one extracted-filename + one user, no duplicates: {rows:?}",
        );
        assert_eq!(rows.iter().filter(|r| r.origin == "extracted").count(), 1,);
        assert_eq!(
            rows.iter()
                .filter(|r| r.origin == "extracted-filename")
                .count(),
            1,
        );
        let user_row = rows
            .iter()
            .find(|r| r.origin == "user")
            .expect("user row preserved");
        assert_eq!(user_row.name, "Hand Curated Editor");
    }
}
