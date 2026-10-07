// SPDX-License-Identifier: Apache-2.0

//! Translation read surface.
//!
//! Backs the `translate.fetch_segment`, `translate.tm_search` and
//! `translate.list_pending` tools: the read face of the translation
//! store. The write tools and the translation audit arrive with later
//! milestones. The `pub(crate)` logic helpers take opened handles; the
//! [`crate::BookrackServer`] tool methods are thin shims that probe and
//! open the stores read-only, dispatch here, and serialize.
//!
//! No store is ever created by this surface. A library that has never
//! been translated into has no `translate.db`, and one without a
//! reference store has no `reference.db`; both read as "no data".

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use bookrack_core::{Explain, Problem};
use bookrack_corpus::{Corpus, CorpusError, Node, NodeId};
use bookrack_distill::Catalogs;
use bookrack_ops::dto::MAX_SPAN_LEAVES;
use bookrack_refs::{Refs, RefsError, ResolvedEntry};
use bookrack_translate::glossary_translations::TranslationRow;
use bookrack_translate::injection::GlossaryHitRow;
use bookrack_translate::pending::{PendingTotals, PendingUnit};
use bookrack_translate::segments::{SegmentRow, span_sha256_hex};
use bookrack_translate::{Translate, TranslateError};
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::reference::{ReferenceError, ReferenceLookupArgs, reference_lookup_logic};

/// Neighbouring segments on each side of the fetched one when the
/// caller does not say.
// setting: translate.neighbors_default
pub const DEFAULT_NEIGHBORS: u32 = 1;

/// Most neighbouring segments a caller may ask for on each side.
// setting: translate.neighbors_max
pub const MAX_NEIGHBORS: u32 = 5;

/// Translation-memory hits returned when the caller does not say.
// setting: translate.tm_limit_default
pub const DEFAULT_TM_LIMIT: u32 = 10;

/// Most translation-memory hits a caller may ask for.
// setting: translate.tm_limit_max
pub const MAX_TM_LIMIT: u32 = 50;

/// Separator between the texts of consecutive leaves and segments.
const TEXT_JOIN: &str = "\n\n";

/// The severity floor applied to reference hits; `info` noise stays
/// out, a flagged entry travels with its flag.
const REFS_MIN_SEVERITY: &str = "warn";

// ---------------------------------------------------------------------------
// Argument and reply shapes
// ---------------------------------------------------------------------------

/// Arguments for `translate.fetch_segment`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TranslateFetchSegmentArgs {
    pub library: Option<String>,
    pub segment_id: i64,
    /// Injection profile for this call only; the unit's own profile
    /// is not changed.
    pub injection_profile: Option<String>,
    /// Neighbouring segments to return on each side, clamped to
    /// `0..=5`; default 1.
    pub neighbors: Option<u32>,
}

/// Texts already recorded on the segment.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CurrentTexts {
    pub draft_text: Option<String>,
    pub reflection_notes: Option<String>,
    pub final_text: Option<String>,
}

/// A pointer to a witness text; the text itself is read through the
/// library read tools.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WitnessPtr {
    pub witness_intake_id: i64,
    pub witness_node_id: i64,
    pub lang: String,
    pub role: String,
    pub note: Option<String>,
}

/// A sealed sibling segment: its source and its final text.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SegmentPair {
    pub segment_id: i64,
    pub source_text: String,
    pub final_text: String,
}

/// One rendering of a glossary term, with the line a prompt quotes.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TranslationChoice {
    pub translation_id: i64,
    pub target_term: Option<String>,
    pub faction: Option<String>,
    pub translator: Option<String>,
    pub citation: Option<String>,
    pub rationale: Option<String>,
    pub authority_ref: Option<String>,
    pub status: String,
    pub inject_hint: String,
}

impl From<TranslationRow> for TranslationChoice {
    fn from(row: TranslationRow) -> Self {
        let inject_hint = row.inject_hint();
        TranslationChoice {
            translation_id: row.translation_id,
            target_term: row.target_term,
            faction: row.faction,
            translator: row.translator,
            citation: row.citation,
            rationale: row.rationale,
            authority_ref: row.authority_ref,
            status: row.status,
            inject_hint,
        }
    }
}

/// One glossary term found in the segment's source text.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GlossaryHit {
    pub term_id: i64,
    pub source_term: String,
    pub term_kind: String,
    pub scope: String,
    pub injection_mode: String,
    /// Half-open char range of the first occurrence in `source_text`.
    pub span_in_source: (usize, usize),
    pub primary: Option<TranslationChoice>,
    pub alternatives: Vec<TranslationChoice>,
}

impl From<GlossaryHitRow> for GlossaryHit {
    fn from(hit: GlossaryHitRow) -> Self {
        GlossaryHit {
            term_id: hit.term.term_id,
            source_term: hit.term.source_term,
            term_kind: hit.term.term_kind,
            scope: hit.term.scope,
            injection_mode: hit.mode.as_str().to_owned(),
            span_in_source: hit.span_in_source,
            primary: hit.primary.map(TranslationChoice::from),
            alternatives: hit
                .alternatives
                .into_iter()
                .map(TranslationChoice::from)
                .collect(),
        }
    }
}

/// A four-part span into the corpus: start leaf and char offset, end
/// leaf and char offset, start inclusive and end exclusive.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SpanRef {
    pub start_node_id: i64,
    pub start_char_offset: i64,
    pub end_node_id: i64,
    pub end_char_offset: i64,
}

/// One translation-memory hit.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TmHit {
    pub segment_id: i64,
    pub intake_id: i64,
    pub span: SpanRef,
    pub src_text: String,
    pub dst_text: String,
    pub source_kind: String,
    pub score: f64,
}

/// Reply of `translate.fetch_segment`: everything a translation prompt
/// for one segment needs, in one package.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TranslateFetchSegmentResult {
    pub segment_id: i64,
    pub unit_id: i64,
    pub intake_id: i64,
    pub target_lang: String,
    pub source_text: String,
    pub unit_outline: String,
    pub neighbors_before: String,
    pub neighbors_after: String,
    pub current_texts: Option<CurrentTexts>,
    pub witnesses: Vec<WitnessPtr>,
    /// `"draft"` or `"review"`.
    pub task_mode: String,
    pub history_in_unit: Vec<SegmentPair>,
    pub glossary_hits: Vec<GlossaryHit>,
    pub refs_hits: Vec<ResolvedEntry>,
    pub tm_hits: Vec<TmHit>,
    pub effective_profile: String,
    /// `"with_glossary"` or `"clean"`.
    pub recommended_prompt_kind: String,
    pub current_status: String,
    pub current_version: i64,
}

/// Arguments for `translate.tm_search`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TranslateTmSearchArgs {
    pub library: Option<String>,
    pub text: String,
    pub source_lang: String,
    pub target_lang: String,
    /// Confine hits to one book's segments.
    pub intake_id_scope: Option<i64>,
    /// Most hits to return, clamped to `1..=50`; default 10.
    pub limit: Option<u32>,
}

/// Reply of `translate.tm_search`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TranslateTmSearchResult {
    pub hits: Vec<TmHit>,
}

/// Arguments for `translate.list_pending`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TranslateListPendingArgs {
    pub library: Option<String>,
    pub intake_id: i64,
    pub target_lang: String,
}

/// One unit that still has unsealed segments.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PendingUnitOut {
    pub unit_id: i64,
    pub node_id: i64,
    pub unit_order: i64,
    pub source_outline: Option<String>,
    pub injection_profile: String,
    pub draft: i64,
    pub proposed: i64,
    pub sealed: i64,
    pub pending_segment_ids: Vec<i64>,
}

impl From<PendingUnit> for PendingUnitOut {
    fn from(unit: PendingUnit) -> Self {
        PendingUnitOut {
            unit_id: unit.unit_id,
            node_id: unit.node_id,
            unit_order: unit.unit_order,
            source_outline: unit.source_outline,
            injection_profile: unit.injection_profile,
            draft: unit.draft,
            proposed: unit.proposed,
            sealed: unit.sealed,
            pending_segment_ids: unit.pending_segment_ids,
        }
    }
}

/// Counts over the whole `(intake, target_lang)` scope.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct PendingTotalsOut {
    pub units: i64,
    pub segments: i64,
    pub draft: i64,
    pub proposed: i64,
    pub sealed: i64,
}

impl From<PendingTotals> for PendingTotalsOut {
    fn from(totals: PendingTotals) -> Self {
        PendingTotalsOut {
            units: totals.units,
            segments: totals.segments,
            draft: totals.draft,
            proposed: totals.proposed,
            sealed: totals.sealed,
        }
    }
}

/// Reply of `translate.list_pending`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TranslateListPendingResult {
    pub intake_id: i64,
    pub target_lang: String,
    pub units: Vec<PendingUnitOut>,
    pub totals: PendingTotalsOut,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors raised by the translate read helpers. The code is decided in
/// `error_map.rs`; the wording comes from [`Explain`].
#[derive(Debug, thiserror::Error)]
pub enum TranslateToolError {
    /// The caller's arguments do not describe a query.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The translation store refused or failed.
    #[error("translation store error")]
    Translate(#[from] TranslateError),

    /// The corpus refused or failed.
    #[error("corpus error")]
    Corpus(#[from] CorpusError),

    /// The reference store refused or failed.
    #[error("reference store error")]
    Refs(#[from] RefsError),

    /// The reference lookup helper refused or failed.
    #[error("reference lookup error")]
    Reference(#[from] ReferenceError),

    /// The source text under a segment's span no longer hashes to the
    /// fingerprint the segment recorded.
    #[error("source text under segment {segment_id} has drifted")]
    SourceDrift {
        /// The segment whose span no longer matches.
        segment_id: i64,
    },

    /// A segment names a unit that does not exist.
    #[error("segment {segment_id} names missing unit {unit_id}")]
    BrokenUnitLink {
        /// The segment carrying the dangling reference.
        segment_id: i64,
        /// The unit id it names.
        unit_id: i64,
    },

    /// A re-slice named a span the unit's leaves do not admit.
    #[error("span does not fit unit {unit_id}: {reason}")]
    SpanOutOfUnit {
        /// The unit being re-sliced.
        unit_id: i64,
        /// What is wrong with the span.
        reason: &'static str,
    },
}

impl Explain for TranslateToolError {
    fn explain(&self) -> Problem {
        match self {
            TranslateToolError::InvalidArgument(what) => {
                Problem::new(format!("cannot run the translation query: {what}"))
            }
            TranslateToolError::Translate(source) => source.explain(),
            TranslateToolError::Corpus(source) => Problem::from_error_chain(source),
            TranslateToolError::Refs(source) => Problem::from_error_chain(source),
            TranslateToolError::Reference(source) => Problem::from_error_chain(source),
            TranslateToolError::SourceDrift { segment_id } => Problem::new(format!(
                "cannot fetch segment {segment_id}: its source text has drifted"
            ))
            .detail(
                "The text under the segment's span no longer hashes to the fingerprint \
                 recorded when the segment was planned; the book was re-ingested or \
                 re-extracted since.",
            )
            .hint("Re-anchoring the segments of a re-ingested book arrives with the reaudit tool."),
            TranslateToolError::BrokenUnitLink {
                segment_id,
                unit_id,
            } => Problem::new(format!(
                "cannot fetch segment {segment_id}: its unit {unit_id} does not exist"
            ))
            .detail(format!(
                "The segment row references unit {unit_id}, but no such row exists; the \
                 store's foreign keys should make that impossible."
            ))
            .hint("Report this with the library name; the translation store needs repair."),
            TranslateToolError::SpanOutOfUnit { unit_id, reason } => {
                Problem::new(format!("cannot re-slice unit {unit_id}: {reason}"))
                    .detail(
                        "Every span must start and end on a leaf of the unit, in chars, start \
                         inclusive and end exclusive, run forwards, stay clear of segments that \
                         carry work, and not overlap another span.",
                    )
                    .hint("Take the unit's leaves and offsets from translate.fetch_segment or library.show_toc.")
            }
        }
    }
}

pub(crate) type ToolResult<T> = Result<T, TranslateToolError>;

// ---------------------------------------------------------------------------
// Store probing
// ---------------------------------------------------------------------------

/// Open the translation store read-only if there is one at `path`.
/// Never creates it.
pub(crate) fn probe_translate(path: &Path) -> ToolResult<Option<Translate>> {
    Ok(Translate::try_open_read_only(path)?)
}

/// Open the reference store read-only if there is one at `path`.
/// Never creates it; a file that exists but cannot be read is an
/// error rather than "no data".
pub(crate) fn probe_refs(path: &Path) -> ToolResult<Option<Refs>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(Refs::open_read_only(path)?))
}

// ---------------------------------------------------------------------------
// translate.fetch_segment
// ---------------------------------------------------------------------------

/// Slice the text a segment spans out of its run of leaves, chars
/// counted per leaf, start inclusive and end exclusive, leaves joined
/// by a blank line.
pub(crate) fn span_text(leaves: &[Node], start_char_offset: i64, end_char_offset: i64) -> String {
    let start = usize::try_from(start_char_offset).unwrap_or(0);
    let end = usize::try_from(end_char_offset).unwrap_or(0);
    let last = leaves.len().saturating_sub(1);
    leaves
        .iter()
        .enumerate()
        .map(|(i, leaf)| {
            let chars = leaf.text_content.as_deref().unwrap_or("").chars();
            match (i == 0, i == last) {
                (true, true) => chars.skip(start).take(end.saturating_sub(start)).collect(),
                (true, false) => chars.skip(start).collect(),
                (false, true) => chars.take(end).collect(),
                (false, false) => chars.collect::<String>(),
            }
        })
        .collect::<Vec<String>>()
        .join(TEXT_JOIN)
}

/// Read the source text one segment spans.
fn segment_source_text(corpus: &Corpus, segment: &SegmentRow) -> ToolResult<String> {
    let leaves = corpus.leaves_between(
        NodeId::new(segment.start_node_id),
        NodeId::new(segment.end_node_id),
        MAX_SPAN_LEAVES,
    )?;
    Ok(span_text(
        &leaves,
        segment.start_char_offset,
        segment.end_char_offset,
    ))
}

/// Document-order position of a segment's first leaf, looked up once
/// per node. A node the corpus no longer has sorts last.
fn start_position(
    corpus: &Corpus,
    positions: &mut HashMap<i64, Option<i64>>,
    node_id: i64,
) -> ToolResult<Option<i64>> {
    if let Some(pos) = positions.get(&node_id) {
        return Ok(*pos);
    }
    let pos = corpus
        .get_node(NodeId::new(node_id))?
        .and_then(|n| n.toc_lo);
    positions.insert(node_id, pos);
    Ok(pos)
}

/// Split `refs://<slug>#<key>` at the first `#`. Anything else is not
/// a reference and yields `None`.
pub(crate) fn parse_authority_ref(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("refs://")?;
    let (slug, key) = rest.split_once('#')?;
    if slug.is_empty() || key.is_empty() {
        return None;
    }
    Some((slug.to_owned(), key.to_owned()))
}

/// Resolve the reference entries the hit renderings cite: every
/// well-formed `authority_ref`, looked up once, flagged entries at
/// `warn` or above kept with their flags, `info` noise dropped.
fn assemble_refs_hits(
    refs: Option<&Refs>,
    catalogs: &Catalogs,
    hits: &[GlossaryHitRow],
) -> ToolResult<Vec<ResolvedEntry>> {
    let Some(refs) = refs else {
        return Ok(Vec::new());
    };
    let cited: BTreeSet<(String, String)> = hits
        .iter()
        .flat_map(|hit| hit.primary.iter().chain(hit.alternatives.iter()))
        .filter_map(|r| r.authority_ref.as_deref())
        .filter_map(parse_authority_ref)
        .collect();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (slug, key) in cited {
        let args = ReferenceLookupArgs {
            library: None,
            book: slug,
            entry_key: key,
            fields: None,
            min_severity: Some(REFS_MIN_SEVERITY.to_owned()),
            exclude_books: None,
        };
        for entry in reference_lookup_logic(refs, catalogs, &args)?.hits {
            if seen.insert((entry.book_slug.clone(), entry.entry_key.clone())) {
                out.push(entry);
            }
        }
    }
    Ok(out)
}

/// Build the `translate.fetch_segment` package.
///
/// `None` when there is no translation store or no such segment. The
/// segment's source text is read from the corpus and checked against
/// the recorded fingerprint before anything else is assembled.
pub(crate) fn fetch_segment_logic(
    translate: Option<&Translate>,
    corpus: &Corpus,
    refs: Option<&Refs>,
    catalogs: &Catalogs,
    args: &TranslateFetchSegmentArgs,
) -> ToolResult<Option<TranslateFetchSegmentResult>> {
    let Some(translate) = translate else {
        return Ok(None);
    };
    let Some(segment) = translate.segment(args.segment_id)? else {
        return Ok(None);
    };
    let Some(unit) = translate.unit(segment.unit_id)? else {
        return Err(TranslateToolError::BrokenUnitLink {
            segment_id: segment.segment_id,
            unit_id: segment.unit_id,
        });
    };

    let source_text = segment_source_text(corpus, &segment)?;
    if span_sha256_hex(&source_text) != segment.source_text_sha {
        return Err(TranslateToolError::SourceDrift {
            segment_id: segment.segment_id,
        });
    }

    // Siblings in document order: by the first leaf's position, then
    // by offset within it.
    let mut positions = HashMap::new();
    let mut siblings = Vec::new();
    for sibling in translate.segments_in_unit(unit.unit_id)? {
        let pos = start_position(corpus, &mut positions, sibling.start_node_id)?;
        siblings.push((pos.unwrap_or(i64::MAX), sibling.start_char_offset, sibling));
    }
    siblings.sort_by_key(|(pos, offset, s)| (*pos, *offset, s.segment_id));
    let siblings: Vec<SegmentRow> = siblings.into_iter().map(|(_, _, s)| s).collect();
    let at = siblings
        .iter()
        .position(|s| s.segment_id == segment.segment_id)
        .unwrap_or(0);

    let n = usize::try_from(
        args.neighbors
            .unwrap_or(DEFAULT_NEIGHBORS)
            .min(MAX_NEIGHBORS),
    )
    .unwrap_or(0);
    let mut before = Vec::new();
    for sibling in &siblings[at.saturating_sub(n)..at] {
        before.push(segment_source_text(corpus, sibling)?);
    }
    let mut after = Vec::new();
    for sibling in siblings.iter().skip(at + 1).take(n) {
        after.push(segment_source_text(corpus, sibling)?);
    }

    let mut history_in_unit = Vec::new();
    for sibling in siblings.iter().filter(|s| s.status == "sealed") {
        let Some(final_text) = sibling.final_text.clone() else {
            continue;
        };
        history_in_unit.push(SegmentPair {
            segment_id: sibling.segment_id,
            source_text: segment_source_text(corpus, sibling)?,
            final_text,
        });
    }

    let effective_profile = args
        .injection_profile
        .clone()
        .unwrap_or_else(|| unit.injection_profile.clone());
    let hits = translate.glossary_hits(
        unit.intake_id,
        &unit.target_lang,
        &effective_profile,
        &source_text,
    )?;
    let recommended_prompt_kind = if hits.iter().any(|h| h.mode.injects_per_segment()) {
        "with_glossary"
    } else {
        "clean"
    };
    let refs_hits = assemble_refs_hits(refs, catalogs, &hits)?;

    let witnesses = translate
        .witnesses_for_unit(unit.unit_id)?
        .into_iter()
        .map(|w| WitnessPtr {
            witness_intake_id: w.witness_intake_id,
            witness_node_id: w.witness_node_id,
            lang: w.lang,
            role: w.role,
            note: w.note,
        })
        .collect();

    let current_texts = (segment.draft_text.is_some()
        || segment.reflection_notes.is_some()
        || segment.final_text.is_some())
    .then(|| CurrentTexts {
        draft_text: segment.draft_text.clone(),
        reflection_notes: segment.reflection_notes.clone(),
        final_text: segment.final_text.clone(),
    });

    Ok(Some(TranslateFetchSegmentResult {
        segment_id: segment.segment_id,
        unit_id: unit.unit_id,
        intake_id: unit.intake_id,
        target_lang: unit.target_lang.clone(),
        source_text,
        unit_outline: unit.source_outline.clone().unwrap_or_default(),
        neighbors_before: before.join(TEXT_JOIN),
        neighbors_after: after.join(TEXT_JOIN),
        current_texts,
        witnesses,
        task_mode: segment.task_mode().to_owned(),
        history_in_unit,
        glossary_hits: hits.into_iter().map(GlossaryHit::from).collect(),
        refs_hits,
        tm_hits: Vec::new(),
        effective_profile,
        recommended_prompt_kind: recommended_prompt_kind.to_owned(),
        current_status: segment.status.clone(),
        current_version: segment.version,
    }))
}

// ---------------------------------------------------------------------------
// translate.tm_search
// ---------------------------------------------------------------------------

/// Validate a translation-memory query and answer it. The index
/// arrives with the translation-memory milestone; until then every
/// valid query has no hits, and the argument shape is already fixed.
pub(crate) fn tm_search_logic(args: &TranslateTmSearchArgs) -> ToolResult<TranslateTmSearchResult> {
    if args.text.trim().is_empty() {
        return Err(TranslateToolError::InvalidArgument(
            "`text` is empty".to_owned(),
        ));
    }
    for (name, value) in [
        ("source_lang", &args.source_lang),
        ("target_lang", &args.target_lang),
    ] {
        if value.trim().is_empty() {
            return Err(TranslateToolError::InvalidArgument(format!(
                "`{name}` is empty"
            )));
        }
    }
    if let Some(intake_id) = args.intake_id_scope
        && intake_id <= 0
    {
        return Err(TranslateToolError::InvalidArgument(format!(
            "`intake_id_scope` {intake_id} is not a book id; intake ids are positive"
        )));
    }
    let _limit = args
        .limit
        .unwrap_or(DEFAULT_TM_LIMIT)
        .clamp(1, MAX_TM_LIMIT);
    Ok(TranslateTmSearchResult { hits: Vec::new() })
}

// ---------------------------------------------------------------------------
// translate.list_pending
// ---------------------------------------------------------------------------

/// Units of one book and target language that still carry unsealed
/// segments. Without a translation store there is nothing pending and
/// the totals are zero.
pub(crate) fn list_pending_logic(
    translate: Option<&Translate>,
    args: &TranslateListPendingArgs,
) -> ToolResult<TranslateListPendingResult> {
    if args.target_lang.trim().is_empty() {
        return Err(TranslateToolError::InvalidArgument(
            "`target_lang` is empty".to_owned(),
        ));
    }
    let (units, totals) = match translate {
        Some(translate) => translate.list_pending(args.intake_id, &args.target_lang)?,
        None => (Vec::new(), PendingTotals::default()),
    };
    Ok(TranslateListPendingResult {
        intake_id: args.intake_id,
        target_lang: args.target_lang.clone(),
        units: units.into_iter().map(PendingUnitOut::from).collect(),
        totals: totals.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_corpus::{NewNode, NodeType};
    use bookrack_refs::{NewBook, NewEntry};
    use serde_json::json;

    /// Three leaves under one chapter at positions 0..=2. Returns the
    /// leaf ids in document order.
    fn seed_corpus(corpus: &mut Corpus, intake_id: i64, texts: [&str; 3]) -> Vec<NodeId> {
        let partition = corpus.allocate_partition(intake_id).expect("partition");
        let root = partition.book_root_id;
        corpus
            .insert_node(&NewNode::root(root, NodeType::Work).title("A Book"))
            .expect("root");
        let ids = corpus.allocate_node_ids(partition.idx, 4).expect("ids");
        corpus
            .insert_node(
                &NewNode::child(ids[0], root, root, 0, 1, NodeType::Chapter)
                    .title("Chapter One")
                    .toc_span(0, 2),
            )
            .expect("chapter");
        for (i, text) in texts.iter().enumerate() {
            let pos = i64::try_from(i).expect("small");
            corpus
                .insert_node(
                    &NewNode::child(ids[i + 1], ids[0], root, pos, 2, NodeType::Paragraph)
                        .text(*text)
                        .text_stats(i64::try_from(text.chars().count()).expect("small"), 1)
                        .toc_span(pos, pos),
                )
                .expect("leaf");
        }
        ids[1..].to_vec()
    }

    /// Seed one row through plain SQL and return the id it reports.
    fn sql(conn: &rusqlite::Connection, statement: &str, params: &[&dyn rusqlite::ToSql]) -> i64 {
        conn.query_row(statement, params, |row| row.get(0))
            .expect("seed row")
    }

    fn unit(conn: &rusqlite::Connection, intake_id: i64, node_id: i64, profile: &str) -> i64 {
        sql(
            conn,
            "INSERT INTO translate_units (intake_id, target_lang, node_id, unit_order, \
             source_outline, injection_profile) VALUES (?1, 'zh', ?2, 0, 'I > 1', ?3) \
             RETURNING unit_id",
            &[&intake_id, &node_id, &profile],
        )
    }

    fn segment(
        conn: &rusqlite::Connection,
        unit_id: i64,
        from: (NodeId, i64),
        to: (NodeId, i64),
        sha: &str,
        status: &str,
    ) -> i64 {
        sql(
            conn,
            "INSERT INTO translate_segments (unit_id, start_node_id, start_char_offset, \
             end_node_id, end_char_offset, source_text_sha, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING segment_id",
            &[
                &unit_id,
                &from.0.get(),
                &from.1,
                &to.0.get(),
                &to.1,
                &sha,
                &status,
            ],
        )
    }

    fn term(conn: &rusqlite::Connection, source_term: &str, kind: &str) -> i64 {
        sql(
            conn,
            "INSERT INTO glossary_terms (scope, scope_ref, source_lang, source_term, \
             source_norm, term_kind) VALUES ('library', NULL, 'de', ?1, lower(?1), ?2) \
             RETURNING term_id",
            &[&source_term, &kind],
        )
    }

    fn rendering(
        conn: &rusqlite::Connection,
        term_id: i64,
        target_term: &str,
        authority_ref: Option<&str>,
    ) -> i64 {
        sql(
            conn,
            "INSERT INTO glossary_translations (term_id, target_lang, target_term, status, \
             authority_ref, proposed_at) VALUES (?1, 'zh', ?2, 'active', ?3, \
             '2026-01-01T00:00:00Z') RETURNING translation_id",
            &[&term_id, &target_term, &authority_ref],
        )
    }

    fn seed_refs() -> Refs {
        let refs = Refs::open_in_memory().expect("refs");
        refs.upsert_book(&NewBook {
            book_slug: "lexicon".into(),
            schema_name: "name_translation".into(),
            schema_version: 1,
            parser_version: "0.1.0".into(),
            title_zh: "lexicon".into(),
            title_en: None,
            edition: None,
            publisher: None,
            year: None,
            isbn: None,
            authority_rank: 10,
            built_at: "2026-06-25T00:00:00Z".into(),
            intake_id: None,
        })
        .expect("book");
        let entry = |key: &str, payload: serde_json::Value, flags: Vec<&str>| NewEntry {
            book_slug: "lexicon".into(),
            entry_key: key.into(),
            headword: key.into(),
            aliases: vec![],
            payload,
            fts_text: key.into(),
            source: json!({
                "book_slug": "lexicon",
                "page": 1,
                "sheet": 1,
                "distill_run_id": "2026-06-25T00:00:00Z",
            }),
            quality_flags: flags.into_iter().map(String::from).collect(),
        };
        refs.upsert_entry(&entry("dasein", json!({"gloss": "being-there"}), vec![]))
            .expect("entry");
        refs.upsert_entry(&entry("loop_a", json!({"redirect_to": "loop_b"}), vec![]))
            .expect("entry");
        refs.upsert_entry(&entry("loop_b", json!({"redirect_to": "loop_a"}), vec![]))
            .expect("entry");
        refs
    }

    fn catalogs() -> Catalogs {
        Catalogs::load_all().expect("catalogs")
    }

    fn fetch(segment_id: i64) -> TranslateFetchSegmentArgs {
        TranslateFetchSegmentArgs {
            library: None,
            segment_id,
            injection_profile: None,
            neighbors: None,
        }
    }

    /// One book of three leaves, one unit, three segments: a sealed
    /// one on leaf 0, the fetched one on leaf 1, a draft one on leaf 2.
    /// The translation store is a real file, written through the
    /// writable open once and then seeded over a plain connection, so
    /// every read in a test goes through the read-only door.
    struct Fixture {
        _dir: tempfile::TempDir,
        translate_db: std::path::PathBuf,
        conn: rusqlite::Connection,
        corpus: Corpus,
        unit_id: i64,
        sealed: i64,
        current: i64,
        later: i64,
    }

    impl Fixture {
        fn translate(&self) -> Translate {
            Translate::open_read_only(&self.translate_db).expect("read-only open")
        }
    }

    fn fixture(profile: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let translate_db = dir.path().join("translate.db");
        drop(Translate::open(&translate_db).expect("create"));
        let conn = rusqlite::Connection::open(&translate_db).expect("seed connection");
        conn.pragma_update(None, "foreign_keys", "ON")
            .expect("pragma");

        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let leaves = seed_corpus(
            &mut corpus,
            1,
            ["Erster Absatz.", "Das Dasein ist Sorge.", "Dritter Absatz."],
        );
        let unit_id = unit(&conn, 1, leaves[0].get(), profile);
        let whole = |text: &str| (0, i64::try_from(text.chars().count()).expect("small"));
        let (lo, hi) = whole("Erster Absatz.");
        let sealed = segment(
            &conn,
            unit_id,
            (leaves[0], lo),
            (leaves[0], hi),
            &span_sha256_hex("Erster Absatz."),
            "sealed",
        );
        conn.execute(
            "UPDATE translate_segments SET final_text = 'first paragraph' WHERE segment_id = ?1",
            [sealed],
        )
        .expect("seal");
        let (lo, hi) = whole("Das Dasein ist Sorge.");
        let current = segment(
            &conn,
            unit_id,
            (leaves[1], lo),
            (leaves[1], hi),
            &span_sha256_hex("Das Dasein ist Sorge."),
            "draft",
        );
        let (lo, hi) = whole("Dritter Absatz.");
        let later = segment(
            &conn,
            unit_id,
            (leaves[2], lo),
            (leaves[2], hi),
            &span_sha256_hex("Dritter Absatz."),
            "draft",
        );
        Fixture {
            _dir: dir,
            translate_db,
            conn,
            corpus,
            unit_id,
            sealed,
            current,
            later,
        }
    }

    #[test]
    fn a_whole_leaf_segment_fetches_every_part_of_the_package() {
        let f = fixture("academic");
        let dasein = term(&f.conn, "Dasein", "term");
        let primary = rendering(&f.conn, dasein, "ci zai", Some("refs://lexicon#dasein"));
        f.conn
            .execute(
                "UPDATE glossary_terms SET primary_choice_id = ?1 WHERE term_id = ?2",
                [primary, dasein],
            )
            .expect("primary");
        f.conn
            .execute(
                "INSERT INTO translate_unit_witnesses (unit_id, witness_intake_id, \
                 witness_node_id, lang, role) VALUES (?1, 9, 90, 'en', 'prior_translation')",
                [f.unit_id],
            )
            .expect("witness");
        let refs = seed_refs();

        let translate = f.translate();
        let got = fetch_segment_logic(
            Some(&translate),
            &f.corpus,
            Some(&refs),
            &catalogs(),
            &fetch(f.current),
        )
        .expect("fetch")
        .expect("package");

        assert_eq!(got.segment_id, f.current);
        assert_eq!(got.unit_id, f.unit_id);
        assert_eq!(got.intake_id, 1);
        assert_eq!(got.target_lang, "zh");
        assert_eq!(got.source_text, "Das Dasein ist Sorge.");
        assert_eq!(got.unit_outline, "I > 1");
        assert_eq!(got.neighbors_before, "Erster Absatz.");
        assert_eq!(got.neighbors_after, "Dritter Absatz.");
        assert_eq!(got.current_texts, None);
        assert_eq!(got.witnesses.len(), 1);
        assert_eq!(got.witnesses[0].witness_node_id, 90);
        assert_eq!(got.task_mode, "draft");
        assert_eq!(
            got.history_in_unit,
            vec![SegmentPair {
                segment_id: f.sealed,
                source_text: "Erster Absatz.".into(),
                final_text: "first paragraph".into(),
            }]
        );
        assert_eq!(got.glossary_hits.len(), 1);
        let hit = &got.glossary_hits[0];
        assert_eq!(hit.term_id, dasein);
        assert_eq!(hit.injection_mode, "primary_plus_alts");
        assert_eq!(hit.span_in_source, (4, 10));
        assert_eq!(
            hit.primary.as_ref().map(|p| p.inject_hint.as_str()),
            Some("ci zai")
        );
        assert_eq!(got.refs_hits.len(), 1);
        assert_eq!(got.refs_hits[0].entry_key, "dasein");
        assert!(got.tm_hits.is_empty());
        assert_eq!(got.effective_profile, "academic");
        assert_eq!(got.recommended_prompt_kind, "with_glossary");
        assert_eq!(got.current_status, "draft");
        assert_eq!(got.current_version, 1);
    }

    #[test]
    fn a_segment_spanning_three_leaves_is_sliced_at_both_ends_and_joined() {
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        let leaves = seed_corpus(&mut corpus, 1, ["abc\u{e9}def", "middle", "ghi jkl"]);
        let dir = tempfile::tempdir().expect("tempdir");
        let translate_db = dir.path().join("translate.db");
        drop(Translate::open(&translate_db).expect("create"));
        let conn = rusqlite::Connection::open(&translate_db).expect("seed connection");
        let unit_id = unit(&conn, 1, leaves[0].get(), "default");
        let expected = "\u{e9}def\n\nmiddle\n\nghi";
        let id = segment(
            &conn,
            unit_id,
            (leaves[0], 3),
            (leaves[2], 3),
            &span_sha256_hex(expected),
            "draft",
        );

        let translate = Translate::open_read_only(&translate_db).expect("read-only open");
        let got = fetch_segment_logic(Some(&translate), &corpus, None, &catalogs(), &fetch(id))
            .expect("fetch")
            .expect("package");
        assert_eq!(got.source_text, expected);
        assert_eq!(got.neighbors_before, "");
        assert_eq!(got.neighbors_after, "");
    }

    #[test]
    fn a_drifted_source_text_is_refused_before_anything_else_is_assembled() {
        let f = fixture("default");
        f.conn
            .execute(
                "UPDATE translate_segments SET source_text_sha = 'stale' WHERE segment_id = ?1",
                [f.current],
            )
            .expect("stale");
        let translate = f.translate();
        let err = fetch_segment_logic(
            Some(&translate),
            &f.corpus,
            None,
            &catalogs(),
            &fetch(f.current),
        )
        .expect_err("drift");
        assert!(
            matches!(err, TranslateToolError::SourceDrift { segment_id } if segment_id == f.current),
            "{err:?}"
        );
        let problem = err.explain();
        assert!(problem.summary.contains("drifted"), "{}", problem.summary);
        assert!(problem.data.hint.is_some());
    }

    #[test]
    fn an_unknown_segment_is_none() {
        let f = fixture("default");
        let translate = f.translate();
        let got = fetch_segment_logic(Some(&translate), &f.corpus, None, &catalogs(), &fetch(404))
            .expect("fetch");
        assert_eq!(got, None);
    }

    #[test]
    fn neighbors_are_clamped_to_the_allowed_range() {
        let f = fixture("default");
        let translate = f.translate();
        let with = |neighbors: Option<u32>| {
            fetch_segment_logic(
                Some(&translate),
                &f.corpus,
                None,
                &catalogs(),
                &TranslateFetchSegmentArgs {
                    neighbors,
                    ..fetch(f.current)
                },
            )
            .expect("fetch")
            .expect("package")
        };
        let none = with(Some(0));
        assert_eq!(none.neighbors_before, "");
        assert_eq!(none.neighbors_after, "");
        let many = with(Some(99));
        assert_eq!(many.neighbors_before, "Erster Absatz.");
        assert_eq!(many.neighbors_after, "Dritter Absatz.");
    }

    #[test]
    fn a_per_call_profile_applies_without_being_written_back() {
        let f = fixture("prose");
        term(&f.conn, "Dasein", "term");
        let translate = f.translate();
        let args = TranslateFetchSegmentArgs {
            injection_profile: Some("academic".into()),
            ..fetch(f.current)
        };
        let got = fetch_segment_logic(Some(&translate), &f.corpus, None, &catalogs(), &args)
            .expect("fetch")
            .expect("package");
        assert_eq!(got.effective_profile, "academic");
        assert_eq!(got.glossary_hits.len(), 1);
        assert_eq!(
            translate
                .unit(f.unit_id)
                .expect("read")
                .expect("row")
                .injection_profile,
            "prose"
        );

        let err = fetch_segment_logic(
            Some(&translate),
            &f.corpus,
            None,
            &catalogs(),
            &TranslateFetchSegmentArgs {
                injection_profile: Some("verbose".into()),
                ..fetch(f.current)
            },
        )
        .expect_err("unknown profile");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::UnknownProfile { .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn chapter_head_only_hits_recommend_the_clean_prompt() {
        let f = fixture("academic");
        term(&f.conn, "Sorge", "proper_noun");
        let translate = f.translate();
        let got = fetch_segment_logic(
            Some(&translate),
            &f.corpus,
            None,
            &catalogs(),
            &fetch(f.current),
        )
        .expect("fetch")
        .expect("package");
        assert_eq!(got.glossary_hits.len(), 1);
        assert_eq!(got.glossary_hits[0].injection_mode, "chapter_head");
        assert_eq!(got.recommended_prompt_kind, "clean");
    }

    #[test]
    fn malformed_authority_refs_are_skipped_and_a_redirect_loop_flag_reaches_the_package() {
        let f = fixture("academic");
        let dasein = term(&f.conn, "Dasein", "term");
        rendering(&f.conn, dasein, "ci zai", Some("not a ref"));
        rendering(&f.conn, dasein, "yuan zai", Some("refs://#dasein"));
        rendering(&f.conn, dasein, "qin zai", Some("refs://lexicon#loop_a"));
        let refs = seed_refs();

        let translate = f.translate();
        let got = fetch_segment_logic(
            Some(&translate),
            &f.corpus,
            Some(&refs),
            &catalogs(),
            &fetch(f.current),
        )
        .expect("fetch")
        .expect("package");
        assert_eq!(got.refs_hits.len(), 1, "{:?}", got.refs_hits);
        assert_eq!(got.refs_hits[0].entry_key, "loop_a");
        assert!(
            got.refs_hits[0]
                .quality_flags
                .iter()
                .any(|f| f == bookrack_refs::REDIRECT_LOOP_FLAG),
            "{:?}",
            got.refs_hits[0].quality_flags
        );
    }

    #[test]
    fn imported_text_not_yet_sealed_is_reviewed_and_its_texts_travel() {
        let f = fixture("default");
        f.conn
            .execute(
                "UPDATE translate_segments SET draft_text = 'imported', source_kind = 'imported', \
                 status = 'proposed' WHERE segment_id = ?1",
                [f.current],
            )
            .expect("import");
        let translate = f.translate();
        let got = fetch_segment_logic(
            Some(&translate),
            &f.corpus,
            None,
            &catalogs(),
            &fetch(f.current),
        )
        .expect("fetch")
        .expect("package");
        assert_eq!(got.task_mode, "review");
        assert_eq!(
            got.current_texts,
            Some(CurrentTexts {
                draft_text: Some("imported".into()),
                reflection_notes: None,
                final_text: None,
            })
        );
        assert_eq!(got.current_status, "proposed");
    }

    #[test]
    fn list_pending_reports_units_and_totals_in_the_wire_shape() {
        let f = fixture("default");
        let translate = f.translate();
        let got = list_pending_logic(
            Some(&translate),
            &TranslateListPendingArgs {
                library: None,
                intake_id: 1,
                target_lang: "zh".into(),
            },
        )
        .expect("list");
        assert_eq!(got.intake_id, 1);
        assert_eq!(got.units.len(), 1);
        assert_eq!(got.units[0].unit_id, f.unit_id);
        assert_eq!(got.units[0].pending_segment_ids, vec![f.current, f.later]);
        assert_eq!(
            got.totals,
            PendingTotalsOut {
                units: 1,
                segments: 3,
                draft: 2,
                proposed: 0,
                sealed: 1,
            }
        );

        let err = list_pending_logic(
            Some(&translate),
            &TranslateListPendingArgs {
                library: None,
                intake_id: 1,
                target_lang: " ".into(),
            },
        )
        .expect_err("empty lang");
        assert!(
            matches!(err, TranslateToolError::InvalidArgument(_)),
            "{err:?}"
        );
    }

    #[test]
    fn tm_search_validates_its_arguments_and_answers_with_no_hits() {
        let args = |text: &str, source_lang: &str, target_lang: &str, limit: Option<u32>| {
            TranslateTmSearchArgs {
                library: None,
                text: text.into(),
                source_lang: source_lang.into(),
                target_lang: target_lang.into(),
                intake_id_scope: None,
                limit,
            }
        };
        let scoped = TranslateTmSearchArgs {
            intake_id_scope: Some(0),
            ..args("Dasein", "de", "zh", None)
        };
        let err = tm_search_logic(&scoped).expect_err("non-positive scope");
        assert!(
            matches!(err, TranslateToolError::InvalidArgument(_)),
            "{err:?}"
        );
        assert!(
            tm_search_logic(&TranslateTmSearchArgs {
                intake_id_scope: Some(3),
                ..args("Dasein", "de", "zh", None)
            })
            .expect("scoped")
            .hits
            .is_empty()
        );
        let got = tm_search_logic(&args("Dasein", "de", "zh", None)).expect("search");
        assert!(got.hits.is_empty());
        assert!(
            tm_search_logic(&args("Dasein", "de", "zh", Some(0)))
                .expect("clamped")
                .hits
                .is_empty()
        );
        for bad in [
            args(" ", "de", "zh", None),
            args("Dasein", "", "zh", None),
            args("Dasein", "de", "", None),
        ] {
            let err = tm_search_logic(&bad).expect_err("invalid");
            assert!(
                matches!(err, TranslateToolError::InvalidArgument(_)),
                "{err:?}"
            );
        }
    }

    #[test]
    fn missing_stores_read_as_no_data_and_are_not_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let translate_db = dir.path().join("translate.db");
        let reference_db = dir.path().join("reference.db");
        let mut corpus = Corpus::open_in_memory().expect("corpus");
        seed_corpus(&mut corpus, 1, ["a", "b", "c"]);

        let translate = probe_translate(&translate_db).expect("probe");
        assert!(translate.is_none());
        let refs = probe_refs(&reference_db).expect("probe");
        assert!(refs.is_none());

        let got = fetch_segment_logic(
            translate.as_ref(),
            &corpus,
            refs.as_ref(),
            &catalogs(),
            &fetch(1),
        )
        .expect("fetch");
        assert_eq!(got, None);
        let got = list_pending_logic(
            translate.as_ref(),
            &TranslateListPendingArgs {
                library: None,
                intake_id: 1,
                target_lang: "zh".into(),
            },
        )
        .expect("list");
        assert!(got.units.is_empty());
        assert_eq!(got.totals, PendingTotalsOut::default());

        assert!(
            !translate_db.exists(),
            "a read must not create translate.db"
        );
        assert!(
            !reference_db.exists(),
            "a read must not create reference.db"
        );

        // A translation store with renderings that cite a reference
        // store that does not exist: the citations resolve to nothing.
        let f = fixture("academic");
        let dasein = term(&f.conn, "Dasein", "term");
        rendering(&f.conn, dasein, "ci zai", Some("refs://lexicon#dasein"));
        let translate = f.translate();
        let got = fetch_segment_logic(
            Some(&translate),
            &f.corpus,
            refs.as_ref(),
            &catalogs(),
            &fetch(f.current),
        )
        .expect("fetch")
        .expect("package");
        assert_eq!(got.glossary_hits.len(), 1);
        assert!(got.refs_hits.is_empty());
        assert!(!reference_db.exists());
    }

    #[test]
    fn a_reference_store_that_exists_but_cannot_be_read_is_an_error_not_no_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let reference_db = dir.path().join("reference.db");
        std::fs::write(&reference_db, b"").expect("empty file");
        let Err(err) = probe_refs(&reference_db) else {
            panic!("an unreadable reference store must not read as no data");
        };
        assert!(matches!(err, TranslateToolError::Refs(_)), "{err:?}");
    }

    #[test]
    fn the_translate_store_path_agrees_with_the_config_layout() {
        let data_root = Path::new("root");
        assert_eq!(
            bookrack_config::translate_db_in(data_root),
            bookrack_config::Config::new(data_root.to_path_buf(), String::new()).translate_db()
        );
    }
}
