// SPDX-License-Identifier: Apache-2.0

//! bookrack-glean: the paper-side pipeline.
//!
//! A peer of `bookrack-ingest` for the books pipeline. The two share a
//! source-file extractor (`bookrack-extract`) and a storage stack
//! (corpus + catalog + vector store) but otherwise stay on separate
//! crates so a change in one cannot quietly reach the other. Five
//! stages match the books pipeline at a high level — extract, register,
//! identify (the glean-specific addition), structure, chunk and embed —
//! but each stage runs paper-shaped logic against paper-shaped state
//! (papers_corpus, papers_catalog, lancedb_papers, papers store).
//!
//! The crate has no `bookrack-ingest` dependency. That is enforced
//! mechanically by the dependency graph, not just discipline.

use std::path::Path;
use std::time::Instant;

use bookrack_catalog::{
    ActorKind, Catalog, IntakeStatus, NewContributor, NewIntake, NewItemPipelineAudit,
    NewItemState, NewPublicationAttrs, NewReview, STATUS_PENDING,
};
use bookrack_config::EmbedConfig;
use bookrack_core::{ItemKind, NodeId, NodeType, PartitionIdx};
use bookrack_corpus::{Corpus, IndexStamps, NewNode};
use bookrack_embed::Embedder;
use bookrack_extract::{
    Block, BlockKind, Contributor, ContributorRole, ExtractOutcome, envelope_filename,
    write_envelope,
};
use bookrack_normalize::{NORMALIZE_VERSION, norm_text_sha256};
use bookrack_vectors::{ChunkRow, ChunkStore};
use sha2::{Digest, Sha256};

pub mod audit;
pub mod dryrun;
pub mod identify;
pub mod rebuild;
pub mod reembed;
pub mod reset;
pub mod stamps;

/// Chunking-behaviour stamp the paper pipeline writes into the vector
/// store's index_meta. Independent of `bookrack_ingest::CHUNK_VERSION`:
/// the two pipelines have separate fleets and bump on their own
/// cadence.
pub const CHUNK_VERSION: u32 = 1;

/// Tuning parameters for chunking the abstract. The defaults are the
/// same target / overlap pair the book pipeline uses; the abstract is
/// usually short enough to produce a single chunk under these
/// settings.
#[derive(Debug, Clone)]
pub struct ChunkParams {
    pub target_chars: usize,
    pub overlap_chars: usize,
}

impl Default for ChunkParams {
    fn default() -> Self {
        Self {
            target_chars: 1000,
            overlap_chars: 100,
        }
    }
}

/// Which way the IDENTIFY pass picks the abstract body. Only
/// [`AbstractStrategy::HeadingFirst`] is implemented; the other variants
/// are reserved for later milestones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AbstractStrategy {
    /// Heading-anchored first, then the first long paragraph on page
    /// one, then the first long paragraph in the body. The default.
    #[default]
    HeadingFirst,
}

/// Which portion of the paper is embedded. Only
/// [`EmbedStrategy::AbstractOnly`] is implemented in this milestone; the
/// other variants reserve names for follow-up work and return an error
/// at the head of `glean_paper` if selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmbedStrategy {
    /// Embed the abstract body only — the cited-passage retrieval the
    /// agent uses lands on the abstract alone in the milestone.
    #[default]
    AbstractOnly,
    /// Embed nothing. Reserved.
    None,
    /// Embed survey-aware sections. Reserved.
    SurveyAware,
}

/// Enrichment knob: identify uses local extraction only when `Off`,
/// reserves names for CrossRef and OpenAlex pulls otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Enrichment {
    /// No enrichment beyond what the local pass found. The default.
    #[default]
    Off,
    /// Pull bibliographic detail from the public CrossRef API. Reserved.
    Crossref,
    /// Pull bibliographic detail from the public OpenAlex API. Reserved.
    OpenAlex,
}

/// Parameters that shape one [`glean_paper`] run.
#[derive(Debug, Clone)]
pub struct GleanParams {
    pub abstract_strategy: AbstractStrategy,
    pub embed_strategy: EmbedStrategy,
    pub enrichment: Enrichment,
    pub chunk: ChunkParams,
    pub embed: EmbedConfig,
    /// Re-glean even when the source is already on file and at status
    /// `Embedded` with current stamps.
    pub force: bool,
    /// Copy the source PDF's bytes into `papers_dir` alongside the
    /// envelope and record the absolute path in
    /// `intake.source_pdf_path`. Defaults to `true`; setting `false`
    /// skips the byte archive and leaves `papers.fetch_source`
    /// returning [`OpsError::SourceNotArchived`] for this intake.
    pub keep_source_pdf: bool,
    /// Extraction-side audit profile shared with the books pipeline.
    /// Consumed by `bookrack_extract::extract` for PDF text-layer
    /// thresholds, EPUB rules, etc. — concerns that live below any
    /// pipeline-shape decision.
    pub extract_profile: bookrack_audit_profile::AuditProfile,
    /// Multi-language chapter / volume heading patterns the TXT
    /// adapter consults. Shared with the books pipeline; consumed by
    /// `bookrack_extract::extract`.
    pub heading_patterns: bookrack_audit_profile::HeadingPatterns,
    /// Paper-shape metadata audit profile. Drives the per-field
    /// grading the glean pipeline runs after `write_biblio`.
    pub paper_audit_profile: audit::PaperAuditProfile,
    /// Runtime data lists the paper audit consults (venue whitelist,
    /// placeholder titles, watermark tokens, sentinel contributor
    /// names).
    pub paper_audit_data: audit::PaperAuditData,
}

impl Default for GleanParams {
    fn default() -> Self {
        Self {
            abstract_strategy: AbstractStrategy::default(),
            embed_strategy: EmbedStrategy::default(),
            enrichment: Enrichment::default(),
            chunk: ChunkParams::default(),
            embed: EmbedConfig::default(),
            force: false,
            keep_source_pdf: true,
            extract_profile: bookrack_audit_profile::AuditProfile::default_profile(),
            heading_patterns: bookrack_audit_profile::HeadingPatterns::default_patterns(),
            paper_audit_profile: audit::PaperAuditProfile::default_profile(),
            paper_audit_data: audit::PaperAuditData::default_data(),
        }
    }
}

/// Outcome of one [`glean_paper`] run.
#[derive(Debug, Clone)]
pub struct GleanReport {
    pub intake_id: i64,
    pub work_node_id: NodeId,
    pub nodes_written: usize,
    pub chunks_written: usize,
    /// `true` when the source was already on file at the time of the
    /// call.
    pub already_registered: bool,
    /// `true` when the run short-circuited because the catalog already
    /// holds an embedded paper for these inputs with matching stamps.
    pub no_op: bool,
    /// `true` when the caller asked for a forced re-run via
    /// `GleanParams::force`.
    pub forced: bool,
    /// DOI carried by the file's own metadata or surfaced by the
    /// IDENTIFY pass. `None` when no DOI was found.
    pub doi: Option<String>,
    /// arXiv identifier in canonical form (no `arXiv:` prefix, no
    /// version suffix). `None` when no arXiv id was found.
    pub arxiv_id: Option<String>,
    /// Container title — journal, conference proceedings, or book
    /// series. Populated from `Biblio::container_title` or the venue
    /// cue scan over the footer.
    pub venue: Option<String>,
    /// Source label of the abstract pick:
    /// `"heading" | "first_page_long_para" | "first_long_para"`.
    /// `None` when no body block could serve as the abstract.
    pub abstract_source: Option<String>,
    /// Audit verdict stored on `node_publication_attrs` for this
    /// paper. `None` when the row has no verdict (audit not run yet,
    /// or the profile disabled the audit).
    pub audit_verdict: Option<String>,
    /// Audit confidence (`high` / `medium` / `low`) stored on
    /// `node_publication_attrs`. `None` when the row has no
    /// confidence.
    pub audit_confidence: Option<String>,
}

/// What went wrong during a glean run. Carries the source variant so
/// callers can act on the failure shape (e.g. distinguish a missing
/// OCR text layer from a database error).
#[derive(Debug, thiserror::Error)]
pub enum GleanError {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("extract error")]
    Extract(#[from] bookrack_extract::ExtractError),
    #[error("catalog error")]
    Catalog(#[from] bookrack_catalog::CatalogError),
    #[error("corpus error")]
    Corpus(#[from] bookrack_corpus::CorpusError),
    #[error("vector store error")]
    Vectors(#[from] bookrack_vectors::VectorsError),
    #[error("embed error")]
    Embed(#[from] bookrack_embed::EmbedError),
    #[error("source file's text layer is unusable: {reason}")]
    NeedsOcr { reason: String },
    #[error("embed strategy `{label}` is not implemented in this milestone")]
    UnimplementedEmbedStrategy { label: &'static str },
    #[error("enrichment `{label}` is not implemented in this milestone")]
    UnimplementedEnrichment { label: &'static str },
    #[error("unknown intake: {0}")]
    UnknownIntake(i64),
    #[error("intake {0} is not in a rebuildable / re-embeddable state")]
    IntakeNotRebuildable(i64),
    #[error("embedder returned no vector")]
    EmptyEmbedding,
}

/// Convenience alias for the crate's fallible operations.
pub type Result<T> = std::result::Result<T, GleanError>;

/// Glean one paper end to end. The whole-file SHA-256 keys the intake;
/// re-gleaning the same file reuses its intake and replaces its corpus
/// tree and vector rows rather than duplicating them. Re-glean is
/// idempotent when the file is at `Embedded` with current stamps;
/// pass `params.force = true` to bypass that check.
#[tracing::instrument(
    name = "paper",
    skip_all,
    fields(file = %file.display(), intake_id = tracing::field::Empty)
)]
pub async fn glean_paper<E: Embedder>(
    file: &Path,
    corpus: &mut Corpus,
    catalog: &mut Catalog,
    lancedb_dir: &Path,
    papers_dir: &Path,
    embedder: &E,
    params: &GleanParams,
) -> Result<GleanReport> {
    // Reject the reserved strategy / enrichment variants up front rather
    // than letting them silently fall back to a default.
    match params.embed_strategy {
        EmbedStrategy::AbstractOnly => {}
        EmbedStrategy::None => {
            return Err(GleanError::UnimplementedEmbedStrategy { label: "none" });
        }
        EmbedStrategy::SurveyAware => {
            return Err(GleanError::UnimplementedEmbedStrategy {
                label: "survey-aware",
            });
        }
    }
    match params.enrichment {
        Enrichment::Off => {}
        Enrichment::Crossref => {
            return Err(GleanError::UnimplementedEnrichment { label: "crossref" });
        }
        Enrichment::OpenAlex => {
            return Err(GleanError::UnimplementedEnrichment { label: "openalex" });
        }
    }

    let bytes = std::fs::read(file)?;
    let source_sha = sha256_hex(&bytes);
    let run_id = new_run_id(&source_sha);

    // No-op fast path: the source is already on file at `Embedded`
    // with stamps matching what this binary would write.
    if !params.force
        && let Some(report) = noop_if_up_to_date(catalog, &source_sha, &params.embed.model)?
    {
        tracing::info!(
            intake_id = report.intake_id,
            "glean noop: source unchanged and stamps current",
        );
        return Ok(report);
    }

    // ── EXTRACT ───────────────────────────────────────────────────────
    let started = Instant::now();
    let extracted =
        bookrack_extract::extract(file, &params.extract_profile, &params.heading_patterns);
    let mut extraction = match extracted {
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
            return Err(GleanError::NeedsOcr { reason });
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
    // Paper-side structuring pass: color the block stream with heading
    // and caption classifications using the PDF outline first, then a
    // text-pattern + geometry heuristic over `BlockStyle`. The PDF
    // adapter is the only producer of `BlockStyle`, so other adapters
    // pass through with `SourceOfStructure::None`.
    let sos = if extraction.provenance.adapter == "pdf" {
        Some(bookrack_extract::pdf_paper::extract_paper_structured(
            &mut extraction.blocks,
            &extraction.toc,
        ))
    } else {
        None
    };
    extraction.provenance.source_of_structure = sos;
    audit(
        catalog,
        &run_id,
        &source_sha,
        None,
        "extract",
        "extract",
        "ok",
        started,
        sos.map(|s| format!(r#"{{"source_of_structure":"{s:?}"}}"#)),
        None,
    );

    // ── REGISTER ──────────────────────────────────────────────────────
    let started = Instant::now();
    let adapter = extraction.provenance.adapter.clone();
    let registration = catalog.register_intake(
        ItemKind::Paper,
        &NewIntake::new(source_sha.clone())
            .format(adapter.clone())
            .byte_size(bytes.len() as i64)
            .original_path(file.to_string_lossy().into_owned()),
    )?;
    let already_registered = !registration.is_new();
    let intake_id = registration.intake().intake_id;
    tracing::Span::current().record("intake_id", intake_id);

    catalog.set_extraction(
        ItemKind::Paper,
        intake_id,
        &extraction.provenance.adapter,
        extraction.provenance.extractor_version,
    )?;
    catalog.set_intake_status(ItemKind::Paper, intake_id, IntakeStatus::Extracted)?;

    let envelope_path = papers_dir.join(envelope_filename(ItemKind::Paper, intake_id));
    match write_envelope(&envelope_path, &extraction, intake_id, &source_sha) {
        Ok(()) => {
            if let Err(err) = catalog.set_stored_path(
                ItemKind::Paper,
                intake_id,
                envelope_path.to_string_lossy().as_ref(),
            ) {
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

    // Archive the source PDF bytes alongside the envelope so downstream
    // tools (raster render, forensic re-open, external viewer) can
    // locate the original file by intake id. Failures here degrade to
    // a `warn` — the envelope remains the authoritative replay record.
    let source_pdf_archived = if params.keep_source_pdf {
        let pdf_path = papers_dir.join(format!("paper-{intake_id}.pdf"));
        match std::fs::copy(file, &pdf_path) {
            Ok(_) => {
                let abs = pdf_path.canonicalize().unwrap_or_else(|_| pdf_path.clone());
                match catalog.set_source_pdf_path(
                    ItemKind::Paper,
                    intake_id,
                    abs.to_string_lossy().as_ref(),
                ) {
                    Ok(_) => true,
                    Err(err) => {
                        tracing::warn!(
                            intake_id,
                            error = %err,
                            "failed to record source_pdf_path; fetch_source unavailable for this intake"
                        );
                        false
                    }
                }
            }
            Err(err) => {
                tracing::warn!(
                    intake_id,
                    error = %err,
                    "failed to copy source PDF bytes; fetch_source unavailable for this intake"
                );
                false
            }
        }
    } else {
        false
    };

    audit(
        catalog,
        &run_id,
        &source_sha,
        None,
        "register",
        "register",
        "ok",
        started,
        Some(format!(r#"{{"pdf_archived":{source_pdf_archived}}}"#)),
        None,
    );

    // ── IDENTIFY ──────────────────────────────────────────────────────
    let started = Instant::now();
    let metadata_text = if extraction.provenance.adapter == "pdf" {
        bookrack_extract::extract_paper_metadata_text(file)
            .ok()
            .flatten()
    } else {
        None
    };
    let filename_stem = file.file_stem().map(|s| s.to_string_lossy().into_owned());
    let mut biblio = extraction.biblio.clone();
    // Title sniff overrides the PDF /Info /Title field unconditionally:
    // template-rendered titles (`PLME0208_696-701.indd`, rotated arXiv
    // banners) are noisier than `None` for the downstream metadata
    // audit.
    biblio.title = identify::sniff_title(biblio.title.as_deref());
    if biblio.doi.is_none() {
        biblio.doi = identify::detect_doi(metadata_text.as_deref(), filename_stem.as_deref());
    }
    if biblio.arxiv_id.is_none() {
        biblio.arxiv_id = identify::detect_arxiv_id(
            extraction.biblio.title.as_deref(),
            metadata_text.as_deref(),
            filename_stem.as_deref(),
        );
    }
    if biblio.container_title.is_none() {
        biblio.container_title = identify::detect_venue(metadata_text.as_deref());
    }
    if biblio.issn.is_none() {
        biblio.issn = identify::detect_issn(metadata_text.as_deref());
    }
    biblio.year = identify::detect_year_from_biblio(
        biblio.arxiv_id.as_deref(),
        biblio.doi.as_deref(),
        &biblio,
        metadata_text.as_deref(),
    );
    let abstract_pick = identify::extract_abstract(file, &extraction, params.abstract_strategy);
    let abstract_source = abstract_pick.as_ref().map(|(_, src)| (*src).to_string());
    if let Some((text, _)) = &abstract_pick
        && biblio.abstract_text.is_none()
    {
        biblio.abstract_text = Some(text.clone());
    }
    audit(
        catalog,
        &run_id,
        &source_sha,
        None,
        "identify",
        "identify",
        "ok",
        started,
        Some(format!(
            r#"{{"doi":{},"arxiv":{},"venue":{},"abstract":{}}}"#,
            biblio.doi.is_some(),
            biblio.arxiv_id.is_some(),
            biblio.container_title.is_some(),
            abstract_pick.is_some(),
        )),
        None,
    );

    // ── STRUCTURE ─────────────────────────────────────────────────────
    let started = Instant::now();
    let abstract_text = abstract_pick.map(|(text, _)| text);
    let structure = build_structure(corpus, intake_id, abstract_text, &extraction.blocks)?;
    audit(
        catalog,
        &run_id,
        &source_sha,
        Some(structure.work_node_id.get()),
        "structure",
        "structure",
        "ok",
        started,
        Some(format!(
            r#"{{"nodes":{},"leaves":{},"body_leaves":{},"sections":{},"subsections":{},"headings":{}}}"#,
            structure.nodes_written,
            if structure.has_leaf { 1 } else { 0 },
            structure.body_leaves,
            structure.section_count,
            structure.subsection_count,
            structure.heading_leaves,
        )),
        None,
    );
    write_biblio(catalog, intake_id, &biblio)?;

    // ── METADATA AUDIT ────────────────────────────────────────────────
    let metadata_started = Instant::now();
    let (audit_verdict, audit_confidence) = run_paper_audit_substep(
        catalog,
        intake_id,
        structure.work_node_id.get(),
        &biblio,
        &extraction.provenance,
        &extraction.blocks,
        file,
        &params.paper_audit_profile,
        &params.paper_audit_data,
        &run_id,
        &source_sha,
        metadata_started,
    )?;

    let parsed_at = catalog.now_iso()?;
    catalog.upsert_book_state(
        &NewItemState::new(structure.work_node_id.get(), intake_id, "structure")
            .parsed_at(&parsed_at),
    )?;

    // ── CHUNK + EMBED ─────────────────────────────────────────────────
    let started = Instant::now();
    let chunks_written = if let Some(leaf_id) = structure.leaf_node_id
        && let Some(text) = structure.leaf_text.as_deref()
    {
        let plans = plan_chunks(leaf_id, text, &params.chunk);
        embed_and_write_chunks(
            corpus,
            lancedb_dir,
            embedder,
            &params.embed,
            intake_id,
            &plans,
        )
        .await?
    } else {
        0
    };
    audit(
        catalog,
        &run_id,
        &source_sha,
        Some(structure.work_node_id.get()),
        "embed",
        "embed",
        "ok",
        started,
        Some(format!(r#"{{"chunks":{chunks_written}}}"#)),
        None,
    );

    let embedded_at = catalog.now_iso()?;
    catalog.upsert_book_state(
        &NewItemState::new(structure.work_node_id.get(), intake_id, "embed")
            .parsed_at(&parsed_at)
            .embedded_at(&embedded_at)
            .embed_model(&params.embed.model),
    )?;
    catalog.set_intake_status(ItemKind::Paper, intake_id, IntakeStatus::Embedded)?;

    Ok(GleanReport {
        intake_id,
        work_node_id: structure.work_node_id,
        nodes_written: structure.nodes_written,
        chunks_written,
        already_registered,
        no_op: false,
        forced: params.force,
        doi: biblio.doi,
        arxiv_id: biblio.arxiv_id,
        venue: biblio.container_title,
        abstract_source,
        audit_verdict,
        audit_confidence,
    })
}

/// Five-stage paper pipeline runs match the books pipeline's audit
/// shape: same actor kind, same sub-step labels, and a glean-tagged
/// detail so a mixed audit log stays attributable.
// The arg list mirrors the catalog's pipeline-audit row directly; a
// helper struct here would just shift the same field set into a
// per-call literal one site over, without simplifying the call.
#[allow(clippy::too_many_arguments)]
pub(crate) fn audit(
    catalog: &Catalog,
    run_id: &str,
    source_sha: &str,
    work_node_id: Option<i64>,
    stage: &str,
    sub_step: &str,
    outcome: &str,
    started: Instant,
    metric_summary: Option<String>,
    error_message: Option<&str>,
) {
    audit_as(
        catalog,
        "glean",
        run_id,
        source_sha,
        work_node_id,
        stage,
        sub_step,
        outcome,
        started,
        metric_summary,
        error_message,
    );
}

/// Variant of [`audit`] that takes an explicit `actor_detail` label so
/// maintenance modules (rebuild / reembed / reset) can distinguish
/// their rows from the initial glean run in a mixed audit log.
#[allow(clippy::too_many_arguments)]
pub(crate) fn audit_as(
    catalog: &Catalog,
    actor_detail: &str,
    run_id: &str,
    source_sha: &str,
    work_node_id: Option<i64>,
    stage: &str,
    sub_step: &str,
    outcome: &str,
    started: Instant,
    metric_summary: Option<String>,
    error_message: Option<&str>,
) {
    let duration_ms = started.elapsed().as_millis() as i64;
    let mut row = NewItemPipelineAudit::new(stage, sub_step, outcome, run_id, ActorKind::Pipeline);
    row.source_sha256 = Some(source_sha.to_string());
    row.duration_ms = Some(duration_ms);
    row.actor_detail = Some(actor_detail.to_string());
    row.book_root_id = work_node_id;
    row.metric_summary = metric_summary;
    row.error_message = error_message.map(|s| s.to_string());
    if let Err(e) = catalog.record_pipeline_audit(&row) {
        tracing::warn!(error = %e, "failed to record pipeline audit row");
    }
}

/// Outcome of the paper STRUCTURE step. `leaf_node_id` / `leaf_text`
/// point at the abstract leaf — the Tier 1 vector anchor — and stay
/// independent of the body leaf count so the downstream CHUNK + EMBED
/// flow sees the same input as before body leaves existed.
pub(crate) struct StructureResult {
    pub(crate) work_node_id: NodeId,
    pub(crate) leaf_node_id: Option<NodeId>,
    pub(crate) leaf_text: Option<String>,
    pub(crate) nodes_written: usize,
    pub(crate) has_leaf: bool,
    pub(crate) body_leaves: usize,
    /// Section organizing nodes written under the Work root. Zero when
    /// the heading pass produced no candidates and the tree fell back
    /// to the flat Phase-1 shape.
    pub(crate) section_count: usize,
    /// Subsection organizing nodes written under any Section.
    pub(crate) subsection_count: usize,
    /// Heading leaves carrying titled text (one per Section /
    /// Subsection plus any depth-3+ heading still folded in as a
    /// leaf). Independent of the organizer counts above.
    pub(crate) heading_leaves: usize,
}

/// One planned node in the paper STRUCTURE pass. The flat plan vector
/// is preorder: every entry's `parent_idx` points to an earlier entry,
/// and leaves precede the next organizer of the same depth. The Work
/// root itself is not in the vector — it is allocated by partition
/// before any planning runs.
struct PendingNode {
    parent_idx: Option<usize>,
    node_type: NodeType,
    depth: i64,
    text: Option<String>,
    source_unit: Option<u32>,
    content_namespace: Option<String>,
    is_leaf: bool,
}

impl PendingNode {
    fn leaf(
        parent_idx: Option<usize>,
        node_type: NodeType,
        depth: i64,
        text: String,
        source_unit: u32,
        content_namespace: String,
    ) -> Self {
        Self {
            parent_idx,
            node_type,
            depth,
            text: Some(text),
            source_unit: Some(source_unit),
            content_namespace: Some(content_namespace),
            is_leaf: true,
        }
    }

    /// Abstract leaf: same prose-leaf shape as `leaf`, except no
    /// per-leaf source-page bounds. The abstract sits above the page
    /// stream — extracting it inside IDENTIFY does not bind it to a
    /// particular page — so its `pages_lo` / `pages_hi` are left
    /// `NULL`. This is the Tier-1 vector-anchor invariant: the
    /// abstract leaf's row matches Phase 1 bit for bit.
    fn abstract_leaf(text: String, content_namespace: String) -> Self {
        Self {
            parent_idx: None,
            node_type: NodeType::Paragraph,
            depth: 1,
            text: Some(text),
            source_unit: None,
            content_namespace: Some(content_namespace),
            is_leaf: true,
        }
    }

    fn organizer(parent_idx: Option<usize>, node_type: NodeType, depth: i64) -> Self {
        Self {
            parent_idx,
            node_type,
            depth,
            text: None,
            source_unit: None,
            content_namespace: None,
            is_leaf: false,
        }
    }
}

/// Pick the deepest open organizer as the parent for a new leaf and
/// report its depth. Returns `(None, 0)` (the Work root, depth 0) when
/// neither a Section nor a Subsection is open.
fn current_parent(subsection: Option<usize>, section: Option<usize>) -> (Option<usize>, i64) {
    if let Some(idx) = subsection {
        (Some(idx), 2)
    } else if let Some(idx) = section {
        (Some(idx), 1)
    } else {
        (None, 0)
    }
}

/// Build the paper's tree. Walks the heading-colored block stream with
/// a small state machine: a `Heading{1}` block opens a Section under
/// the Work root, a `Heading{2}` block opens a Subsection under the
/// current Section (or a new auto-opened Section when none is
/// outstanding), Body / Caption / depth-3+ Heading blocks attach as
/// the matching prose-leaf type under the deepest open organizer (or
/// the Work root in the SourceOfStructure::None fallback). Abstract
/// blocks are skipped — the abstract leaf is already pushed first,
/// from IDENTIFY's output, and is the Tier 1 vector anchor.
///
/// The abstract leaf's NodeId, content namespace, text / norm hashes,
/// and (toc, page) span are bit-for-bit identical to the Phase-1
/// shape: it is the first allocated id after the Work root, sits at
/// depth 1 directly under it, and carries the same
/// `intake:{intake_id}:abstract` namespace. Body-leaf namespaces
/// continue to count Body blocks only, so a re-glean of an
/// uncolored Phase-1 envelope still produces the same body hashes.
pub(crate) fn build_structure(
    corpus: &mut Corpus,
    intake_id: i64,
    abstract_text: Option<String>,
    body_blocks: &[Block],
) -> Result<StructureResult> {
    let partition_idx = PartitionIdx::new(intake_id);
    corpus.drop_partition(partition_idx)?;
    let partition = corpus.allocate_partition(intake_id)?;
    let work_node_id = partition.book_root_id;

    let abstract_trimmed = abstract_text
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    // Plan the tree as a flat preorder list. Each entry carries enough
    // metadata to materialize a NewNode later; parents are referenced
    // by index into the same Vec so an organizer can be patched with
    // its descendants' page / toc span once they are known.
    let mut plans: Vec<PendingNode> = Vec::new();
    let mut abstract_plan_idx: Option<usize> = None;

    if let Some(text) = abstract_trimmed {
        let plan_idx = plans.len();
        plans.push(PendingNode::abstract_leaf(
            text,
            format!("intake:{intake_id}:abstract"),
        ));
        abstract_plan_idx = Some(plan_idx);
    }

    let mut current_section: Option<usize> = None;
    let mut current_subsection: Option<usize> = None;
    let mut section_seq: i64 = 0;
    let mut subsection_within_section: i64 = 0;
    let mut body_seq: i64 = 0;
    let mut caption_seq: i64 = 0;
    let mut heading_leaf_seq: i64 = 0;
    let mut section_count = 0usize;
    let mut subsection_count = 0usize;
    let mut heading_leaves = 0usize;
    let mut body_leaves = 0usize;

    for block in body_blocks {
        let trimmed = block.text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if matches!(block.kind, BlockKind::Abstract) {
            // Already promoted to the leaf above; the colored block
            // remains in the envelope, but it does not enter the tree
            // a second time.
            continue;
        }
        match block.kind {
            BlockKind::Heading { level } if level <= 1 => {
                current_subsection = None;
                section_seq += 1;
                subsection_within_section = 0;
                let section_idx = plans.len();
                plans.push(PendingNode::organizer(None, NodeType::Section, 1));
                plans.push(PendingNode::leaf(
                    Some(section_idx),
                    NodeType::Heading,
                    2,
                    trimmed.to_string(),
                    block.source_unit,
                    format!("intake:{intake_id}:heading:{section_seq}"),
                ));
                current_section = Some(section_idx);
                section_count += 1;
                heading_leaves += 1;
            }
            BlockKind::Heading { level: 2 } => {
                let section_idx = match current_section {
                    Some(idx) => idx,
                    None => {
                        section_seq += 1;
                        subsection_within_section = 0;
                        let idx = plans.len();
                        plans.push(PendingNode::organizer(None, NodeType::Section, 1));
                        current_section = Some(idx);
                        section_count += 1;
                        idx
                    }
                };
                subsection_within_section += 1;
                let subsection_idx = plans.len();
                plans.push(PendingNode::organizer(
                    Some(section_idx),
                    NodeType::Subsection,
                    2,
                ));
                plans.push(PendingNode::leaf(
                    Some(subsection_idx),
                    NodeType::Heading,
                    3,
                    trimmed.to_string(),
                    block.source_unit,
                    format!("intake:{intake_id}:heading:{section_seq}.{subsection_within_section}"),
                ));
                current_subsection = Some(subsection_idx);
                subsection_count += 1;
                heading_leaves += 1;
            }
            BlockKind::Heading { .. } => {
                heading_leaf_seq += 1;
                let (parent, depth) = current_parent(current_subsection, current_section);
                plans.push(PendingNode::leaf(
                    parent,
                    NodeType::Heading,
                    depth + 1,
                    trimmed.to_string(),
                    block.source_unit,
                    format!("intake:{intake_id}:heading-leaf:{heading_leaf_seq}"),
                ));
                heading_leaves += 1;
            }
            BlockKind::Body => {
                let (parent, depth) = current_parent(current_subsection, current_section);
                plans.push(PendingNode::leaf(
                    parent,
                    NodeType::Paragraph,
                    depth + 1,
                    trimmed.to_string(),
                    block.source_unit,
                    format!("intake:{intake_id}:body:{body_seq}"),
                ));
                body_seq += 1;
                body_leaves += 1;
            }
            // BlockKind::Footnote and BlockKind::Other are not produced
            // by the paper PDF adapter and are not part of the paper
            // STRUCTURE contract; drop them silently rather than have
            // unrelated callers surface them as paper-side leaves.
            BlockKind::Footnote | BlockKind::Other => continue,
            BlockKind::Caption => {
                caption_seq += 1;
                let (parent, depth) = current_parent(current_subsection, current_section);
                plans.push(PendingNode::leaf(
                    parent,
                    NodeType::FigureCaption,
                    depth + 1,
                    trimmed.to_string(),
                    block.source_unit,
                    format!("intake:{intake_id}:caption:{caption_seq}"),
                ));
            }
            BlockKind::Abstract => unreachable!("filtered above"),
        }
    }

    // Allocate one NodeId per planned node, then walk the plans in
    // order to compute child indices, leaf preorder positions, and
    // organizer (toc, page) spans.
    let ids = if plans.is_empty() {
        Vec::new()
    } else {
        corpus.allocate_node_ids(partition_idx, plans.len() as u32)?
    };

    // child_index per plan: count siblings under the same parent
    // earlier in the Vec.
    let mut child_index_of: Vec<i64> = vec![0; plans.len()];
    let mut child_count_under: std::collections::HashMap<Option<usize>, i64> =
        std::collections::HashMap::new();
    for (i, plan) in plans.iter().enumerate() {
        let count = child_count_under.entry(plan.parent_idx).or_insert(0);
        child_index_of[i] = *count;
        *count += 1;
    }

    // Leaf preorder positions. Organizers get None; leaves get their
    // running index over leaves only.
    let mut leaf_position: Vec<Option<i64>> = vec![None; plans.len()];
    let mut leaf_cursor: i64 = 0;
    for (i, plan) in plans.iter().enumerate() {
        if plan.is_leaf {
            leaf_position[i] = Some(leaf_cursor);
            leaf_cursor += 1;
        }
    }
    let total_leaves = leaf_cursor;

    // Children-of map for organizer span computation.
    let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); plans.len()];
    for (i, plan) in plans.iter().enumerate() {
        if let Some(parent_idx) = plan.parent_idx {
            children_of[parent_idx].push(i);
        }
    }

    // Span aggregation: for each organizer, walk its subtree and take
    // the (min, max) of leaf positions and source_units. The recursion
    // depth is at most three (Work → Section → Subsection → leaf) so
    // an explicit stack would not save much.
    fn subtree_spans(
        idx: usize,
        plans: &[PendingNode],
        children_of: &[Vec<usize>],
        leaf_position: &[Option<i64>],
    ) -> Option<(i64, i64, u32, u32)> {
        let mut span: Option<(i64, i64, u32, u32)> = None;
        if let Some(pos) = leaf_position[idx] {
            let pu = plans[idx].source_unit.unwrap_or(0);
            span = Some((pos, pos, pu, pu));
        }
        for &child in &children_of[idx] {
            if let Some((tlo, thi, plo, phi)) =
                subtree_spans(child, plans, children_of, leaf_position)
            {
                span = match span {
                    None => Some((tlo, thi, plo, phi)),
                    Some((a, b, c, d)) => Some((a.min(tlo), b.max(thi), c.min(plo), d.max(phi))),
                };
            }
        }
        span
    }

    let mut nodes: Vec<NewNode> = Vec::with_capacity(1 + plans.len());

    // Work root. The root spans every leaf when at least one exists;
    // otherwise it is written bare.
    let mut root = NewNode::root(work_node_id, NodeType::Work);
    if total_leaves > 0 {
        root = root.toc_span(0, total_leaves - 1);
    }
    nodes.push(root);

    let mut leaf_node_id: Option<NodeId> = None;
    let mut leaf_text: Option<String> = None;

    for (i, plan) in plans.iter().enumerate() {
        let node_id = ids[i];
        let parent_id = plan.parent_idx.map(|p| ids[p]).unwrap_or(work_node_id);
        let ordinal = child_index_of[i];
        let mut node = NewNode::child(
            node_id,
            parent_id,
            work_node_id,
            ordinal,
            plan.depth,
            plan.node_type,
        );
        if plan.is_leaf {
            let text = plan.text.clone().expect("leaf carries text");
            let char_count = text.chars().count() as i64;
            let pos = leaf_position[i].expect("leaf has a preorder position");
            node = node
                .text(text.clone())
                .text_stats(char_count, 0)
                .toc_span(pos, pos);
            // Leaves without a source unit (the abstract) leave their
            // page bounds NULL; every other leaf collapses
            // `source_unit` into both `pages_lo` and `pages_hi`.
            if let Some(page) = plan.source_unit {
                let page = i64::from(page);
                node = node.pages(page, page);
            }
            // Content hashes are reserved for prose leaves at the
            // corpus boundary. Structural leaves (FigureCaption) skip
            // the anchor / text_sha / norm_sha triple.
            if plan.node_type.is_prose_leaf() {
                let text_sha = sha256_hex(text.as_bytes());
                let norm_sha = norm_text_sha256(&text);
                let namespace = plan
                    .content_namespace
                    .clone()
                    .expect("prose leaf carries a namespace");
                node = node.content_hashes(namespace, text_sha, norm_sha);
            }
            if Some(i) == abstract_plan_idx {
                leaf_node_id = Some(node_id);
                leaf_text = Some(text);
            }
        } else {
            // Organizer: page and toc spans aggregated from descendants.
            if let Some((tlo, thi, plo, phi)) =
                subtree_spans(i, &plans, &children_of, &leaf_position)
            {
                node = node
                    .toc_span(tlo, thi)
                    .pages(i64::from(plo), i64::from(phi));
            }
        }
        nodes.push(node);
    }

    let nodes_written = nodes.len();
    let has_leaf = leaf_node_id.is_some();
    corpus.insert_nodes(&nodes)?;
    Ok(StructureResult {
        work_node_id,
        leaf_node_id,
        leaf_text,
        nodes_written,
        has_leaf,
        body_leaves,
        section_count,
        subsection_count,
        heading_leaves,
    })
}

/// Write the bibliographic columns and contributor rows for a paper.
pub(crate) fn write_biblio(
    catalog: &Catalog,
    intake_id: i64,
    biblio: &bookrack_extract::Biblio,
) -> Result<()> {
    let mut attrs = NewPublicationAttrs::new(intake_id, ItemKind::Paper);
    attrs.title = biblio.title.clone();
    attrs.subtitle = biblio.subtitle.clone();
    attrs.publisher = biblio.publisher.clone();
    attrs.year = biblio.year.map(|y| y.to_string());
    attrs.isbn = biblio.isbn.clone();
    attrs.series = biblio.series.clone();
    attrs.language = biblio.language.clone();
    attrs.doi = biblio.doi.clone();
    attrs.arxiv_id = biblio.arxiv_id.clone();
    attrs.issn = biblio.issn.clone();
    attrs.container_title = biblio.container_title.clone();
    attrs.abstract_text = biblio.abstract_text.clone();
    attrs.csl_type = biblio.csl_type.map(|t| serde_csl_type(t).to_string());
    attrs.source = Some("extracted".to_string());
    catalog.upsert_publication_attrs(&attrs)?;
    catalog.clear_extracted_contributors(intake_id, ItemKind::Paper)?;
    for (ordinal, contributor) in biblio.contributors.iter().enumerate() {
        let role_str = role_str(contributor.role);
        let mut new = NewContributor::new(
            intake_id,
            ItemKind::Paper,
            role_str,
            ordinal as i64,
            "extracted",
            contributor_display(contributor),
        );
        if let Some(family) = &contributor.family {
            new = new.family(family.clone());
        }
        if let Some(given) = &contributor.given {
            new = new.given(given.clone());
        }
        if let Some(orcid) = &contributor.orcid {
            new = new.orcid(orcid.clone());
        }
        catalog.add_contributor(&new)?;
    }
    Ok(())
}

fn contributor_display(c: &Contributor) -> String {
    if !c.name.is_empty() {
        return c.name.clone();
    }
    match (c.given.as_deref(), c.family.as_deref()) {
        (Some(g), Some(f)) => format!("{g} {f}"),
        (None, Some(f)) => f.to_string(),
        (Some(g), None) => g.to_string(),
        (None, None) => String::new(),
    }
}

fn role_str(role: ContributorRole) -> &'static str {
    match role {
        ContributorRole::Author => "author",
        ContributorRole::Editor => "editor",
        ContributorRole::Translator => "translator",
        ContributorRole::Other => "other",
    }
}

/// Map a [`bookrack_extract::CslType`] to its CSL serde string.
fn serde_csl_type(t: bookrack_extract::CslType) -> &'static str {
    match t {
        bookrack_extract::CslType::ArticleJournal => "article-journal",
        bookrack_extract::CslType::PaperConference => "paper-conference",
        bookrack_extract::CslType::Book => "book",
        bookrack_extract::CslType::Chapter => "chapter",
        bookrack_extract::CslType::Thesis => "thesis",
        bookrack_extract::CslType::Report => "report",
        bookrack_extract::CslType::Webpage => "webpage",
    }
}

/// Plan the chunks for an abstract leaf. The abstract is almost always
/// one chunk under the default 1000-character target.
pub(crate) fn plan_chunks(leaf_id: NodeId, text: &str, params: &ChunkParams) -> Vec<PlannedChunk> {
    let splitter = text_splitter::TextSplitter::new(
        text_splitter::ChunkConfig::new(params.target_chars)
            .with_overlap(params.overlap_chars)
            .expect("valid chunk config"),
    );
    let mut out = Vec::new();
    let mut cursor = 0i32;
    for piece in splitter.chunks(text) {
        let trimmed = piece.trim();
        if trimmed.is_empty() {
            continue;
        }
        let len = trimmed.chars().count() as i32;
        let start = cursor;
        let end = cursor + len;
        cursor = end;
        out.push(PlannedChunk {
            start_node_id: leaf_id,
            start_char_offset: start,
            end_node_id: leaf_id,
            end_char_offset: end,
            text: trimmed.to_string(),
            norm_chunk_sha256: norm_text_sha256(trimmed),
        });
    }
    out
}

pub(crate) struct PlannedChunk {
    pub(crate) start_node_id: NodeId,
    pub(crate) start_char_offset: i32,
    pub(crate) end_node_id: NodeId,
    pub(crate) end_char_offset: i32,
    pub(crate) text: String,
    pub(crate) norm_chunk_sha256: String,
}

/// Embed each planned chunk and append the rows to the paper vector
/// store. Reconciles the store's index_meta with this pipeline's
/// stamps on the first write into an empty dir.
pub(crate) async fn embed_and_write_chunks<E: Embedder>(
    corpus: &mut Corpus,
    lancedb_dir: &Path,
    embedder: &E,
    cfg: &EmbedConfig,
    intake_id: i64,
    plans: &[PlannedChunk],
) -> Result<usize> {
    if plans.is_empty() {
        return Ok(0);
    }
    let dim = probe_dimension(embedder).await?;
    let stamps = IndexStamps {
        embed_model: cfg.model.clone(),
        vector_dim: dim as u32,
        chunk_version: CHUNK_VERSION,
        normalize_version: NORMALIZE_VERSION,
    };
    corpus.reconcile_index_stamps(&stamps)?;
    let store = match ChunkStore::try_open(lancedb_dir).await? {
        Some(s) => s,
        None => ChunkStore::open(lancedb_dir, dim).await?,
    };
    // Drop the previous rows for this intake so a re-glean replaces
    // them rather than duplicating.
    store.delete_partition(PartitionIdx::new(intake_id)).await?;
    let texts: Vec<String> = plans.iter().map(|p| p.text.clone()).collect();
    let vectors = embedder.embed_batch(&texts).await?;
    let mut rows = Vec::with_capacity(plans.len());
    for (plan, vector) in plans.iter().zip(vectors) {
        rows.push(ChunkRow {
            vector,
            text: plan.text.clone(),
            start_node_id: plan.start_node_id,
            start_char_offset: plan.start_char_offset,
            end_node_id: plan.end_node_id,
            end_char_offset: plan.end_char_offset,
            norm_chunk_sha256: plan.norm_chunk_sha256.clone(),
        });
    }
    let written = store.append(&rows).await?;
    Ok(written)
}

async fn probe_dimension<E: Embedder>(embedder: &E) -> Result<usize> {
    let vectors = embedder
        .embed_batch(&["bookrack glean probe".to_string()])
        .await?;
    let first = vectors
        .into_iter()
        .next()
        .ok_or_else(|| GleanError::NeedsOcr {
            reason: "embedder returned no vector for the dimension probe".to_string(),
        })?;
    Ok(first.len())
}

/// Run the paper-side metadata audit after `write_biblio`. Returns
/// `(verdict, confidence)` to populate on `GleanReport`, also
/// writing them through `update_audit_rollup` and a `pending` row
/// onto `node_reviews` whose `reviewed_by` carries the active
/// profile name. Failures inside this step are logged and the
/// surrounding pipeline continues — the audit is consultative, not
/// gating.
#[allow(clippy::too_many_arguments)]
fn run_paper_audit_substep(
    catalog: &Catalog,
    intake_id: i64,
    work_node_id: i64,
    biblio: &bookrack_extract::Biblio,
    provenance: &bookrack_extract::Provenance,
    blocks: &[Block],
    file: &Path,
    profile: &audit::PaperAuditProfile,
    data: &audit::PaperAuditData,
    run_id: &str,
    source_sha: &str,
    started: Instant,
) -> Result<(Option<String>, Option<String>)> {
    let reviewer = format!("bookrack-glean:{}", profile.name);
    if !profile.audit_enabled {
        if let Err(e) = catalog.upsert_review(
            &NewReview::new(intake_id, ItemKind::Paper, &reviewer, STATUS_PENDING).notes(format!(
                "audit skipped: {} profile disables the metadata audit",
                profile.name,
            )),
        ) {
            tracing::warn!(error = %e, "metadata: failed to write node_reviews row");
        }
        audit(
            catalog,
            run_id,
            source_sha,
            Some(work_node_id),
            "metadata",
            "audit",
            "skipped",
            started,
            None,
            None,
        );
        return Ok((None, None));
    }
    let effective = match catalog.effective_publication_attrs(intake_id, ItemKind::Paper) {
        Ok(eff) => eff,
        Err(e) => {
            tracing::warn!(error = %e, "metadata: failed to read effective attrs");
            audit(
                catalog,
                run_id,
                source_sha,
                Some(work_node_id),
                "metadata",
                "read_effective",
                "fail",
                started,
                None,
                Some(&e.to_string()),
            );
            return Ok((None, None));
        }
    };
    let body_sample = paper_body_sample(blocks);
    let source_stem = file.file_stem().and_then(|s| s.to_str());
    let input = audit::PaperAuditInput {
        biblio,
        provenance,
        effective: &effective,
        body_sample: &body_sample,
        source_stem,
    };
    let report = audit::audit_paper(&input, profile, data);
    let confidence = report.confidence.as_token().to_string();
    let verdict = report.verdict.as_token().to_string();
    if let Err(e) = catalog.update_audit_rollup(intake_id, ItemKind::Paper, &confidence, &verdict) {
        tracing::warn!(error = %e, "metadata: failed to write audit rollup");
    }
    if let Err(e) = catalog.upsert_review(
        &NewReview::new(intake_id, ItemKind::Paper, &reviewer, STATUS_PENDING)
            .notes(report.to_json()),
    ) {
        tracing::warn!(error = %e, "metadata: failed to write node_reviews row");
    }
    let outcome = match report.verdict {
        audit::PaperVerdict::Clean => "ok",
        audit::PaperVerdict::NeedsWork => "partial",
    };
    let metric = format!(
        r#"{{"verdict":"{}","confidence":"{}","fields":{}}}"#,
        verdict,
        confidence,
        report.fields.len(),
    );
    audit(
        catalog,
        run_id,
        source_sha,
        Some(work_node_id),
        "metadata",
        "audit",
        outcome,
        started,
        Some(metric),
        None,
    );
    Ok((Some(verdict), Some(confidence)))
}

/// Concatenate text from the first few body blocks, bounded by a
/// character cap, for the audit's language signal.
fn paper_body_sample(blocks: &[Block]) -> String {
    const SAMPLE_BLOCKS: usize = 10;
    const SAMPLE_CHARS: usize = 4096;
    let mut out = String::new();
    for block in blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Body))
        .take(SAMPLE_BLOCKS)
    {
        for ch in block.text.chars() {
            if out.chars().count() >= SAMPLE_CHARS {
                return out;
            }
            out.push(ch);
        }
        out.push('\n');
    }
    out
}

/// Return a [`GleanReport`] when re-running the pipeline against this
/// source would be a no-op: the file is already on file at
/// `Embedded` status, the stored extractor version equals this
/// binary's, and the embed model on the work state matches.
fn noop_if_up_to_date(
    catalog: &Catalog,
    source_sha: &str,
    embed_model: &str,
) -> Result<Option<GleanReport>> {
    let Some(intake) = catalog.intake_by_sha(source_sha)? else {
        return Ok(None);
    };
    if intake.status != IntakeStatus::Embedded {
        return Ok(None);
    }
    if intake.extractor_version != bookrack_extract::EXTRACTOR_VERSION {
        return Ok(None);
    }
    let book_root_id = PartitionIdx::new(intake.intake_id).root().get();
    let Some(state) = catalog.book_state(book_root_id)? else {
        return Ok(None);
    };
    if state.embed_model.as_deref() != Some(embed_model) {
        return Ok(None);
    }
    // Surface the audit outcome stored on the row at the previous
    // glean run so a no-op still tells the operator whether the paper
    // landed `clean` or `needs_work`.
    let attrs = catalog.publication_attrs(intake.intake_id, ItemKind::Paper)?;
    let audit_verdict = attrs.as_ref().and_then(|a| a.audit_verdict.clone());
    let audit_confidence = attrs.as_ref().and_then(|a| a.confidence.clone());
    Ok(Some(GleanReport {
        intake_id: intake.intake_id,
        work_node_id: NodeId::new(state.book_root_id),
        nodes_written: 0,
        chunks_written: 0,
        already_registered: true,
        no_op: true,
        forced: false,
        doi: None,
        arxiv_id: None,
        venue: None,
        abstract_source: None,
        audit_verdict,
        audit_confidence,
    }))
}

/// Hex-encoded SHA-256 of a byte slice — the source identity anchor.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// One run id ties every audit row from this invocation together. The
/// `glean-` prefix distinguishes paper-pipeline rows from ingest's
/// `ingest-` prefix when a mixed log is inspected.
pub(crate) fn new_run_id(source_sha: &str) -> String {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let prefix = source_sha.get(..8).unwrap_or(source_sha);
    format!("glean-{prefix}-{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: &str, source_unit: u32) -> Block {
        Block {
            kind: BlockKind::Body,
            text: text.to_string(),
            source_unit,
            style: None,
        }
    }

    fn other(kind: BlockKind, text: &str, source_unit: u32) -> Block {
        Block {
            kind,
            text: text.to_string(),
            source_unit,
            style: None,
        }
    }

    #[test]
    fn build_structure_emits_abstract_and_body_paragraphs_in_document_order() {
        // No headings in the block stream → the tree falls back to the
        // flat Phase-1 shape: 1 Work root + 1 abstract leaf + N body
        // Paragraph leaves directly under the Work root.
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let intake_id = 42_i64;
        let abstract_text = Some("Synthetic abstract for testing.".to_string());
        let blocks = vec![
            body("First body block on page 0.", 0),
            body("Second body block on page 0.", 0),
            body("Body block on page 1.", 1),
            body("Body block on page 2.", 2),
        ];

        let result = build_structure(&mut corpus, intake_id, abstract_text, &blocks)
            .expect("build_structure");

        assert!(result.has_leaf, "abstract leaf must be present");
        assert_eq!(
            result.body_leaves, 4,
            "every body block becomes a body leaf"
        );
        assert_eq!(result.section_count, 0, "no Section organizer in fallback");
        assert_eq!(result.subsection_count, 0);
        assert_eq!(result.heading_leaves, 0);
        assert_eq!(
            result.nodes_written, 6,
            "1 Work root + 1 abstract leaf + 4 body leaves"
        );

        let work = result.work_node_id;
        let leaves = corpus
            .leaves_in_doc_span(work, 0, i64::from(i32::MAX), 1024)
            .expect("leaves");
        assert_eq!(leaves.len(), 5, "5 leaves total");

        let abstract_leaf = &leaves[0];
        assert_eq!(
            abstract_leaf.stable_anchor.as_deref(),
            Some(format!("intake:{intake_id}:abstract").as_str()),
        );
        assert!(
            abstract_leaf.page_index_start.is_none() && abstract_leaf.page_index_end.is_none(),
            "abstract leaf carries no source-page bounds"
        );
        assert_eq!(
            abstract_leaf.text_content.as_deref(),
            Some("Synthetic abstract for testing.")
        );

        let body_pages: Vec<i64> = leaves
            .iter()
            .skip(1)
            .map(|n| n.page_index_start.expect("body page"))
            .collect();
        assert_eq!(
            body_pages,
            vec![0, 0, 1, 2],
            "body leaves keep extraction order via source_unit",
        );

        for (i, leaf) in leaves.iter().skip(1).enumerate() {
            assert_eq!(
                leaf.stable_anchor.as_deref(),
                Some(format!("intake:{intake_id}:body:{i}").as_str()),
            );
            assert_eq!(
                leaf.page_index_start, leaf.page_index_end,
                "pages_lo / pages_hi share source_unit",
            );
            let toc = leaf.toc_lo.expect("toc_lo");
            assert_eq!(Some(toc), leaf.toc_hi, "toc_lo / toc_hi collapse on a leaf");
            assert_eq!(
                toc,
                i64::try_from(i + 1).unwrap(),
                "toc is monotone after abstract"
            );
        }
    }

    #[test]
    fn build_structure_without_abstract_still_emits_body_leaves() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let intake_id = 7_i64;
        let blocks = vec![body("only body", 0), body("another body", 0)];

        let result = build_structure(&mut corpus, intake_id, None, &blocks).expect("structure");

        assert!(!result.has_leaf, "no abstract leaf");
        assert!(result.leaf_node_id.is_none());
        assert!(result.leaf_text.is_none());
        assert_eq!(result.body_leaves, 2);
        assert_eq!(result.nodes_written, 3, "Work root + 2 body leaves");

        let leaves = corpus
            .leaves_in_doc_span(result.work_node_id, 0, i64::from(i32::MAX), 1024)
            .expect("leaves");
        assert_eq!(leaves.len(), 2);
        assert_eq!(
            leaves[0].stable_anchor.as_deref(),
            Some(format!("intake:{intake_id}:body:0").as_str()),
        );
        assert_eq!(
            leaves[0].toc_lo,
            Some(0),
            "without abstract, body leaves start at toc 0",
        );
    }

    #[test]
    fn build_structure_skips_empty_body_text() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let blocks = vec![body("", 0), body("   \n  ", 0), body("real body text", 1)];

        let result = build_structure(&mut corpus, 99, None, &blocks).expect("structure");

        assert_eq!(result.body_leaves, 1, "blank Body blocks are dropped");
        assert_eq!(result.nodes_written, 2, "Work root + 1 body leaf");
    }

    #[test]
    fn build_structure_sets_root_toc_span_to_cover_every_leaf() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let intake_id = 3_i64;
        let abstract_text = Some("Abstract.".to_string());
        let blocks = vec![body("a", 0), body("b", 1), body("c", 1)];

        let result =
            build_structure(&mut corpus, intake_id, abstract_text, &blocks).expect("structure");

        let root = corpus
            .get_node(result.work_node_id)
            .expect("root row")
            .expect("root present");
        assert_eq!(root.toc_lo, Some(0));
        assert_eq!(
            root.toc_hi,
            Some(3),
            "root span covers abstract (0) plus three body leaves (1..3)",
        );
    }

    #[test]
    fn build_structure_leaves_root_toc_span_null_when_no_leaves() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let result = build_structure(&mut corpus, 4, None, &[]).expect("structure");

        let root = corpus
            .get_node(result.work_node_id)
            .expect("root row")
            .expect("root present");
        assert_eq!(root.toc_lo, None);
        assert_eq!(root.toc_hi, None);
    }

    #[test]
    fn build_structure_with_empty_abstract_string_is_treated_as_absent() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let blocks = vec![body("body", 0)];

        let result =
            build_structure(&mut corpus, 1, Some("   ".to_string()), &blocks).expect("structure");

        assert!(!result.has_leaf, "whitespace-only abstract is dropped");
        assert_eq!(result.body_leaves, 1);
    }

    #[test]
    fn build_structure_assembles_section_tree_from_heading_blocks() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let intake_id = 7_i64;
        let blocks = vec![
            other(BlockKind::Heading { level: 1 }, "1. Introduction", 0),
            body("Intro body line.", 0),
            other(BlockKind::Heading { level: 2 }, "1.1 Motivation", 1),
            body("Motivation body.", 1),
            other(BlockKind::Caption, "Figure 1: example.", 1),
            other(BlockKind::Heading { level: 1 }, "2. Method", 2),
            body("Method body.", 2),
        ];

        let result = build_structure(&mut corpus, intake_id, None, &blocks).expect("structure");

        assert_eq!(
            result.section_count, 2,
            "two Heading{{1}} blocks → two Sections"
        );
        assert_eq!(result.subsection_count, 1);
        assert_eq!(
            result.heading_leaves, 3,
            "two Section + one Subsection heading"
        );
        assert_eq!(result.body_leaves, 3, "three Body blocks");
        // 1 Work root + 2 Section organizers + 1 Subsection organizer
        // + 3 Heading leaves + 3 Paragraph leaves + 1 FigureCaption.
        assert_eq!(result.nodes_written, 11);

        let work = result.work_node_id;
        let children = corpus.children(work).expect("root children");
        let kinds: Vec<NodeType> = children.iter().map(|c| c.node_type).collect();
        assert_eq!(kinds, vec![NodeType::Section, NodeType::Section]);

        // First Section: Heading + Paragraph leaf + a Subsection.
        let first_section = &children[0];
        let first_children = corpus
            .children(first_section.node_id)
            .expect("section children");
        let first_kinds: Vec<NodeType> = first_children.iter().map(|c| c.node_type).collect();
        assert_eq!(
            first_kinds,
            vec![NodeType::Heading, NodeType::Paragraph, NodeType::Subsection]
        );

        // Subsection holds Heading + Paragraph + FigureCaption.
        let subsection = &first_children[2];
        let sub_children = corpus.children(subsection.node_id).expect("sub children");
        let sub_kinds: Vec<NodeType> = sub_children.iter().map(|c| c.node_type).collect();
        assert_eq!(
            sub_kinds,
            vec![
                NodeType::Heading,
                NodeType::Paragraph,
                NodeType::FigureCaption
            ]
        );

        // FigureCaption carries text but no content hashes.
        let caption = &sub_children[2];
        assert_eq!(caption.text_content.as_deref(), Some("Figure 1: example."));
        assert!(caption.norm_text_sha256.is_none());
        assert!(caption.stable_anchor.is_none());

        // Body Paragraph stable_anchor still counts only Body blocks:
        // body:0 inside first Section, body:1 inside Subsection, body:2
        // inside second Section.
        let intro_para = &first_children[1];
        assert_eq!(
            intro_para.stable_anchor.as_deref(),
            Some(format!("intake:{intake_id}:body:0").as_str()),
        );
        let motivation_para = &sub_children[1];
        assert_eq!(
            motivation_para.stable_anchor.as_deref(),
            Some(format!("intake:{intake_id}:body:1").as_str()),
        );

        // Section organizers carry no body text, no content hashes, but
        // their page span aggregates descendants.
        assert!(first_section.text_content.is_none());
        assert!(first_section.stable_anchor.is_none());
        let first_pages = (
            first_section.page_index_start.expect("section pages_lo"),
            first_section.page_index_end.expect("section pages_hi"),
        );
        assert_eq!(first_pages, (0, 1), "first Section spans pages 0..=1");
        // toc spans collapse to leaf preorder positions.
        let first_toc = (
            first_section.toc_lo.expect("section toc_lo"),
            first_section.toc_hi.expect("section toc_hi"),
        );
        assert_eq!(
            first_toc.1 - first_toc.0 + 1,
            5,
            "first Section covers 5 leaves"
        );
    }

    #[test]
    fn build_structure_preserves_abstract_leaf_bit_for_bit_under_heading_path() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let intake_id = 11_i64;
        let abstract_text = Some("The abstract body text.".to_string());
        // Same intake on the same abstract: with and without heading
        // colorings must yield the exact same abstract leaf row.
        let with_heading = vec![
            other(BlockKind::Heading { level: 1 }, "1. Introduction", 0),
            body("Intro body.", 0),
        ];
        let flat = vec![body("Intro body.", 0)];

        let r_with = build_structure(&mut corpus, intake_id, abstract_text.clone(), &with_heading)
            .expect("with-heading");
        let abstract_id_a = r_with.leaf_node_id.expect("abstract leaf");
        let abstract_text_a = r_with.leaf_text.clone();
        let abstract_row_a = corpus
            .get_node(abstract_id_a)
            .expect("get a")
            .expect("row a");

        // Rebuild the structure: drop_partition runs inside
        // build_structure, so calling it again replaces the tree.
        let r_flat =
            build_structure(&mut corpus, intake_id, abstract_text.clone(), &flat).expect("flat");
        let abstract_id_b = r_flat.leaf_node_id.expect("abstract leaf");
        let abstract_row_b = corpus
            .get_node(abstract_id_b)
            .expect("get b")
            .expect("row b");

        assert_eq!(abstract_id_a, abstract_id_b, "same NodeId");
        assert_eq!(
            abstract_row_a.stable_anchor, abstract_row_b.stable_anchor,
            "same stable anchor"
        );
        assert_eq!(
            abstract_row_a.text_sha256, abstract_row_b.text_sha256,
            "same text sha"
        );
        assert_eq!(
            abstract_row_a.norm_text_sha256, abstract_row_b.norm_text_sha256,
            "same norm sha"
        );
        assert_eq!(abstract_row_a.toc_lo, abstract_row_b.toc_lo, "same toc_lo");
        assert_eq!(
            abstract_row_a.page_index_start, abstract_row_b.page_index_start,
            "page bounds: both None"
        );
        assert_eq!(abstract_text_a, r_flat.leaf_text, "same leaf_text");
    }

    #[test]
    fn build_structure_promotes_orphan_subsection_to_a_section() {
        // A Heading{2} block without a preceding Heading{1} auto-opens
        // a Section so the Subsection has a valid parent.
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let blocks = vec![
            other(BlockKind::Heading { level: 2 }, "1.1 Orphan subsection", 0),
            body("Body beneath the orphan subsection.", 0),
        ];

        let result = build_structure(&mut corpus, 3, None, &blocks).expect("structure");

        assert_eq!(result.section_count, 1, "auto-opened Section");
        assert_eq!(result.subsection_count, 1);
        assert_eq!(
            result.heading_leaves, 1,
            "only the Subsection heading was colored"
        );
        let work = result.work_node_id;
        let children = corpus.children(work).expect("children");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].node_type, NodeType::Section);
        let section_children = corpus.children(children[0].node_id).expect("section");
        let kinds: Vec<NodeType> = section_children.iter().map(|c| c.node_type).collect();
        // Auto-opened Section carries only the Subsection — no leading
        // Heading leaf, since no Heading{1} text existed.
        assert_eq!(kinds, vec![NodeType::Subsection]);
    }
}
