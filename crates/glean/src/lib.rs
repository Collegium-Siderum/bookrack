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
    NewItemState, NewPublicationAttrs,
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

pub mod identify;

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
#[derive(Debug, Clone, Default)]
pub struct GleanParams {
    pub abstract_strategy: AbstractStrategy,
    pub embed_strategy: EmbedStrategy,
    pub enrichment: Enrichment,
    pub chunk: ChunkParams,
    pub embed: EmbedConfig,
    /// Re-glean even when the source is already on file and at status
    /// `Embedded` with current stamps.
    pub force: bool,
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
}

/// Parameters for [`dryrun_paper`]. Minimal until the dryrun surface
/// grows.
#[derive(Debug, Clone, Default)]
pub struct DryrunPaperParams {
    pub abstract_strategy: AbstractStrategy,
}

/// Outcome of [`dryrun_paper`] — a no-write inspection of what
/// `glean_paper` would do.
#[derive(Debug, Clone, Default)]
pub struct DryrunPaperReport {
    pub doi: Option<String>,
    pub arxiv_id: Option<String>,
    pub venue: Option<String>,
    pub abstract_source: Option<String>,
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
    let audit_profile = bookrack_audit_profile_default();
    let extracted = bookrack_extract::extract(file, &audit_profile, &Default::default());
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
    audit(
        catalog,
        &run_id,
        &source_sha,
        None,
        "register",
        "register",
        "ok",
        started,
        None,
        None,
    );

    // ── IDENTIFY ──────────────────────────────────────────────────────
    let started = Instant::now();
    let body_text = identify::body_text(&extraction);
    let footer_text = identify::footer_text(&extraction);
    let mut biblio = extraction.biblio.clone();
    if biblio.doi.is_none()
        && let Some(d) = identify::detect_doi(&body_text)
    {
        biblio.doi = Some(d);
    }
    if biblio.arxiv_id.is_none()
        && let Some(a) = identify::detect_arxiv_id(biblio.title.as_deref(), &footer_text)
    {
        biblio.arxiv_id = Some(a);
    }
    if biblio.container_title.is_none()
        && let Some(v) = identify::detect_venue(&footer_text)
    {
        biblio.container_title = Some(v);
    }
    if biblio.issn.is_none()
        && let Some(i) = identify::detect_issn(&footer_text)
    {
        biblio.issn = Some(i);
    }
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
            r#"{{"nodes":{},"leaves":{},"body_leaves":{}}}"#,
            structure.nodes_written,
            if structure.has_leaf { 1 } else { 0 },
            structure.body_leaves,
        )),
        None,
    );
    write_biblio(catalog, intake_id, &biblio)?;
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
    })
}

/// Inspect what `glean_paper` would do without writing anything. Stub
/// until the dryrun surface grows.
pub fn dryrun_paper(_file: &Path, _params: &DryrunPaperParams) -> DryrunPaperReport {
    DryrunPaperReport::default()
}

/// Five-stage paper pipeline runs match the books pipeline's audit
/// shape: same actor kind, same sub-step labels, and a glean-tagged
/// detail so a mixed audit log stays attributable.
// The arg list mirrors the catalog's pipeline-audit row directly; a
// helper struct here would just shift the same field set into a
// per-call literal one site over, without simplifying the call.
#[allow(clippy::too_many_arguments)]
fn audit(
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
    let duration_ms = started.elapsed().as_millis() as i64;
    let mut row = NewItemPipelineAudit::new(stage, sub_step, outcome, run_id, ActorKind::Pipeline);
    row.source_sha256 = Some(source_sha.to_string());
    row.duration_ms = Some(duration_ms);
    row.actor_detail = Some("glean".to_string());
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
struct StructureResult {
    work_node_id: NodeId,
    leaf_node_id: Option<NodeId>,
    leaf_text: Option<String>,
    nodes_written: usize,
    has_leaf: bool,
    body_leaves: usize,
}

/// Build the paper's tree: one Work root, an optional abstract
/// Paragraph leaf, and one Paragraph leaf per non-empty
/// `BlockKind::Body` block in document order. Body leaves carry the
/// raw extracted text and a per-block `pages_lo` / `pages_hi`
/// drawn from `source_unit`; the abstract leaf stays bit-for-bit
/// identical to the pre-Phase-1 shape so the CHUNK + EMBED stage
/// keeps the same vector anchor.
fn build_structure(
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
    let bodies: Vec<&Block> = body_blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Body) && !b.text.trim().is_empty())
        .collect();

    let abstract_count = usize::from(abstract_trimmed.is_some());
    let leaf_count = abstract_count + bodies.len();
    let ids = if leaf_count > 0 {
        corpus.allocate_node_ids(partition_idx, leaf_count as u32)?
    } else {
        Vec::new()
    };

    let mut nodes = Vec::with_capacity(1 + leaf_count);
    nodes.push(NewNode::root(work_node_id, NodeType::Work));

    let mut leaf_node_id: Option<NodeId> = None;
    let mut leaf_text: Option<String> = None;
    let mut id_cursor = 0usize;
    let mut ordinal: i64 = 0;

    if let Some(trimmed) = abstract_trimmed {
        let leaf_id = ids[id_cursor];
        id_cursor += 1;
        let char_count = trimmed.chars().count() as i64;
        let text_sha = sha256_hex(trimmed.as_bytes());
        let norm_sha = norm_text_sha256(&trimmed);
        nodes.push(
            NewNode::child(
                leaf_id,
                work_node_id,
                work_node_id,
                ordinal,
                1,
                NodeType::Paragraph,
            )
            .text(trimmed.clone())
            .text_stats(char_count, 0)
            .toc_span(ordinal, ordinal)
            .content_hashes(format!("intake:{intake_id}:abstract"), text_sha, norm_sha),
        );
        leaf_node_id = Some(leaf_id);
        leaf_text = Some(trimmed);
        ordinal += 1;
    }

    for (body_idx, block) in bodies.iter().enumerate() {
        let trimmed = block.text.trim().to_string();
        let body_id = ids[id_cursor];
        id_cursor += 1;
        let char_count = trimmed.chars().count() as i64;
        let text_sha = sha256_hex(trimmed.as_bytes());
        let norm_sha = norm_text_sha256(&trimmed);
        let page = i64::from(block.source_unit);
        nodes.push(
            NewNode::child(
                body_id,
                work_node_id,
                work_node_id,
                ordinal,
                1,
                NodeType::Paragraph,
            )
            .text(trimmed)
            .text_stats(char_count, 0)
            .toc_span(ordinal, ordinal)
            .pages(page, page)
            .content_hashes(
                format!("intake:{intake_id}:body:{body_idx}"),
                text_sha,
                norm_sha,
            ),
        );
        ordinal += 1;
    }

    let nodes_written = nodes.len();
    let has_leaf = leaf_node_id.is_some();
    let body_leaves = bodies.len();
    corpus.insert_nodes(&nodes)?;
    Ok(StructureResult {
        work_node_id,
        leaf_node_id,
        leaf_text,
        nodes_written,
        has_leaf,
        body_leaves,
    })
}

/// Write the bibliographic columns and contributor rows for a paper.
fn write_biblio(
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
fn plan_chunks(leaf_id: NodeId, text: &str, params: &ChunkParams) -> Vec<PlannedChunk> {
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

struct PlannedChunk {
    start_node_id: NodeId,
    start_char_offset: i32,
    end_node_id: NodeId,
    end_char_offset: i32,
    text: String,
    norm_chunk_sha256: String,
}

/// Embed each planned chunk and append the rows to the paper vector
/// store. Reconciles the store's index_meta with this pipeline's
/// stamps on the first write into an empty dir.
async fn embed_and_write_chunks<E: Embedder>(
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

/// Return a [`GleanReport`] when re-running the pipeline against this
/// source would be a no-op: the file is already on file at
/// `Embedded` status under the same embed model.
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
    let book_root_id = PartitionIdx::new(intake.intake_id).root().get();
    let Some(state) = catalog.book_state(book_root_id)? else {
        return Ok(None);
    };
    if state.embed_model.as_deref() != Some(embed_model) {
        return Ok(None);
    }
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
    }))
}

/// Hex-encoded SHA-256 of a byte slice — the source identity anchor.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// One run id ties every audit row from this invocation together. The
/// `glean-` prefix distinguishes paper-pipeline rows from ingest's
/// `ingest-` prefix when a mixed log is inspected.
fn new_run_id(source_sha: &str) -> String {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let prefix = source_sha.get(..8).unwrap_or(source_sha);
    format!("glean-{prefix}-{nanos}")
}

/// Default audit profile for the glean pipeline. Loaded from the
/// embedded profile crate so glean stays free of the ingest crate.
fn bookrack_audit_profile_default() -> bookrack_audit_profile::AuditProfile {
    bookrack_audit_profile::AuditProfile::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: &str, source_unit: u32) -> Block {
        Block {
            kind: BlockKind::Body,
            text: text.to_string(),
            source_unit,
        }
    }

    fn other(kind: BlockKind, text: &str, source_unit: u32) -> Block {
        Block {
            kind,
            text: text.to_string(),
            source_unit,
        }
    }

    #[test]
    fn build_structure_emits_abstract_and_body_paragraphs_in_document_order() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let intake_id = 42_i64;
        let abstract_text = Some("Synthetic abstract for testing.".to_string());
        let blocks = vec![
            body("First body block on page 0.", 0),
            body("Second body block on page 0.", 0),
            other(BlockKind::Heading { level: 1 }, "1. Introduction", 1),
            body("Body block on page 1.", 1),
            other(BlockKind::Caption, "Figure 1: synthetic.", 1),
            body("Body block on page 2.", 2),
            other(BlockKind::Footnote, "footnote text", 2),
            other(BlockKind::Other, "unclassified text", 2),
        ];

        let result = build_structure(&mut corpus, intake_id, abstract_text, &blocks)
            .expect("build_structure");

        assert!(result.has_leaf, "abstract leaf must be present");
        assert_eq!(result.body_leaves, 4, "only Body blocks become body leaves");
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
    fn build_structure_with_empty_abstract_string_is_treated_as_absent() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let blocks = vec![body("body", 0)];

        let result =
            build_structure(&mut corpus, 1, Some("   ".to_string()), &blocks).expect("structure");

        assert!(!result.has_leaf, "whitespace-only abstract is dropped");
        assert_eq!(result.body_leaves, 1);
    }
}
