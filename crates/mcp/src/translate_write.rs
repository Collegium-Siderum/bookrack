// SPDX-License-Identifier: Apache-2.0

//! The write face of the translation store behind the `translate.*`
//! MCP tools: planning a book's units and segments, re-slicing the
//! virgin segments of one unit, the three-stage proposal, the seal,
//! and the import of an existing translation.
//!
//! Every write is one call, one SQLite transaction and one audit row.
//! The tools open the store through the writable door, so a plan is
//! the one path that creates `translate.db`; the reads keep probing.
//! The corpus is read-only throughout.

use std::collections::{HashMap, HashSet};

use bookrack_catalog::ActorKind;
use bookrack_core::{NodeId, PartitionIdx};
use bookrack_corpus::{Corpus, Node};
use bookrack_ops::Caller;
use bookrack_translate::audit::{AuditSubject, NewAudit};
use bookrack_translate::segmentation::{Script, Triage, script_of, split_at_sentences, triage};
use bookrack_translate::segments::{
    NewSegment, ProposeStage, SOURCE_KIND_IMPORTED, SegmentRow, span_sha256_hex,
};
use bookrack_translate::units::NewUnit;
use bookrack_translate::witnesses::NewWitness;
use bookrack_translate::{Translate, TranslateError};
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::translate::{SpanRef, ToolResult, TranslateToolError, WitnessPtr, span_text};

/// Escape threshold for a leaf in a Latin-script text, in chars.
// setting: translate.plan.max_chars_latin
pub const MAX_CHARS_LATIN: usize = 1200;

/// Escape threshold for a leaf in a CJK-script text, in chars.
// setting: translate.plan.max_chars_cjk
pub const MAX_CHARS_CJK: usize = 600;

/// The escape threshold for a script when the caller does not say.
pub const fn default_max_chars(script: Script) -> usize {
    match script {
        Script::Latin => MAX_CHARS_LATIN,
        Script::Cjk => MAX_CHARS_CJK,
    }
}

/// Who is writing and when: stamped on every audit row a write appends.
#[derive(Debug, Clone, Copy)]
pub struct WriteContext<'a> {
    pub caller: &'a Caller,
    /// RFC 3339 UTC.
    pub now: &'a str,
}

impl WriteContext<'_> {
    fn audit<'b>(
        &'b self,
        subject: AuditSubject,
        action: &'b str,
        reason: Option<&'b str>,
        payload: &'b serde_json::Value,
        cost_tokens: Option<i64>,
    ) -> NewAudit<'b> {
        NewAudit {
            subject,
            action,
            actor_kind: self.caller.actor_kind,
            actor_detail: self.caller.actor_detail.as_deref(),
            session_id: self.caller.session_id.as_deref(),
            reason: reason.or(self.caller.reason.as_deref()),
            payload: Some(payload),
            cost_tokens,
            changed_at: self.now,
        }
    }
}

// ---------------------------------------------------------------------------
// Argument and reply shapes
// ---------------------------------------------------------------------------

/// Arguments for `translate.plan`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TranslatePlanArgs {
    /// The library to plan in. Required: a write never falls back to
    /// the default library.
    pub library: String,
    pub intake_id: i64,
    pub target_lang: String,
    /// Plan only the subtree under this organizing node; the whole book
    /// when absent.
    pub chapter_node_id: Option<i64>,
    /// Escape threshold in chars for every leaf; the per-script default
    /// when absent.
    pub max_chars: Option<usize>,
    /// Injection profile for the units this call creates; `default`
    /// when absent. Existing units keep theirs.
    pub injection_profile: Option<String>,
    /// Witness texts to anchor on the planned units.
    pub witnesses: Option<Vec<WitnessDecl>>,
    /// Recorded on the audit row.
    pub reason: Option<String>,
}

/// One witness book to anchor, unit by unit.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WitnessDecl {
    pub intake_id: i64,
    pub lang: String,
    /// `alt_source`, `translation_witness` or `prior_translation`.
    pub role: String,
    pub note: Option<String>,
    /// Explicit `(unit_node_id, witness_node_id)` pairs. When absent,
    /// the witness book's leaf-bearing nodes align to the planned units
    /// in document order.
    pub chapter_map: Option<Vec<(i64, i64)>>,
}

/// Reply of `translate.plan`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TranslatePlanResult {
    pub intake_id: i64,
    pub target_lang: String,
    pub units: Vec<UnitStub>,
    pub segments: Vec<SegmentStub>,
    pub skipped: Vec<SkipReason>,
    /// The audit row this plan appended.
    pub plan_id: i64,
    pub created_units: u64,
    pub created_segments: u64,
}

/// One planned unit.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UnitStub {
    pub unit_id: i64,
    pub node_id: i64,
    pub unit_order: i64,
    pub source_outline: String,
    pub injection_profile: String,
    /// True when the unit was already there before this call.
    pub existed: bool,
    pub witnesses: Vec<WitnessPtr>,
}

/// One planned segment.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SegmentStub {
    pub segment_id: i64,
    pub unit_id: i64,
    pub span: SpanRef,
    pub char_count: i64,
    /// True when a segment on this span was already there.
    pub existed: bool,
}

/// A leaf the plan did not turn into a segment, and why.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkipReason {
    pub node_id: i64,
    pub node_type: String,
    pub reason: String,
    /// Whether a person should look at the leaf anyway.
    pub needs_attention: bool,
}

/// Arguments for `translate.resegment`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TranslateResegmentArgs {
    /// The library to write. Required.
    pub library: String,
    pub unit_id: i64,
    /// The spans that replace the unit's virgin segments.
    pub new_spans: Vec<SpanRef>,
    /// Recorded on the audit row.
    pub reason: Option<String>,
}

/// Reply of `translate.resegment`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TranslateResegmentResult {
    pub unit_id: i64,
    /// The virgin segments removed.
    pub removed: Vec<i64>,
    /// The segments inserted, in span order.
    pub segments: Vec<SegmentStub>,
    pub audit_id: i64,
}

// ---------------------------------------------------------------------------
// Corpus structure
// ---------------------------------------------------------------------------

/// A book's nodes indexed for the walks a plan makes.
struct Tree<'a> {
    by_id: HashMap<i64, &'a Node>,
    /// Children by parent id, in ordinal order.
    children: HashMap<i64, Vec<&'a Node>>,
}

impl<'a> Tree<'a> {
    fn build(nodes: &'a [Node]) -> Tree<'a> {
        let by_id = nodes.iter().map(|n| (n.node_id.get(), n)).collect();
        let mut children: HashMap<i64, Vec<&Node>> = HashMap::new();
        for node in nodes {
            if let Some(parent) = node.parent_id {
                children.entry(parent.get()).or_default().push(node);
            }
        }
        for siblings in children.values_mut() {
            siblings.sort_by_key(|n| n.ordinal);
        }
        Tree { by_id, children }
    }

    /// The non-organizing children of `node_id`, in ordinal order.
    fn leaves_of(&self, node_id: i64) -> Vec<&'a Node> {
        self.children
            .get(&node_id)
            .map(|c| {
                c.iter()
                    .copied()
                    .filter(|n| !n.node_type.is_organizing())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The heading path from the book root down to `node`, one title
    /// per level, the node type's name standing in for a missing title.
    fn outline(&self, node: &Node) -> String {
        let mut names = Vec::new();
        let mut current = Some(node);
        while let Some(n) = current {
            names.push(
                n.title
                    .clone()
                    .unwrap_or_else(|| n.node_type.as_str().to_owned()),
            );
            current = n.parent_id.and_then(|p| self.by_id.get(&p.get()).copied());
        }
        names.reverse();
        names.join(" > ")
    }

    /// The organizing nodes that directly hold at least one leaf, each
    /// with those leaves, in document order of their first leaf. With
    /// `under`, only the nodes whose span lies inside that node's.
    fn leaf_bearing(
        &self,
        nodes: &'a [Node],
        under: Option<i64>,
    ) -> ToolResult<Vec<(&'a Node, Vec<&'a Node>)>> {
        let scope = match under {
            None => None,
            Some(id) => {
                let node = self.by_id.get(&id).copied().ok_or_else(|| {
                    TranslateToolError::InvalidArgument(format!(
                        "`chapter_node_id` {id} is not a node of this book"
                    ))
                })?;
                if !node.node_type.is_organizing() {
                    return Err(TranslateToolError::InvalidArgument(format!(
                        "`chapter_node_id` {id} is a leaf, not a chapter or section"
                    )));
                }
                Some((node.toc_lo, node.toc_hi))
            }
        };
        let mut units = Vec::new();
        for node in nodes.iter().filter(|n| n.node_type.is_organizing()) {
            if let Some((lo, hi)) = scope {
                let inside = match (node.toc_lo, node.toc_hi, lo, hi) {
                    (Some(nlo), Some(nhi), Some(lo), Some(hi)) => nlo >= lo && nhi <= hi,
                    _ => false,
                };
                if !inside {
                    continue;
                }
            }
            let leaves = self.leaves_of(node.node_id.get());
            if leaves.is_empty() {
                continue;
            }
            units.push((node, leaves));
        }
        units.sort_by_key(|(node, leaves)| {
            (leaves[0].toc_lo.unwrap_or(i64::MAX), node.node_id.get())
        });
        Ok(units)
    }
}

fn book_nodes(corpus: &Corpus, intake_id: i64, what: &str) -> ToolResult<Vec<Node>> {
    let nodes = corpus.book_nodes(PartitionIdx::new(intake_id).root())?;
    if nodes.is_empty() {
        return Err(TranslateToolError::InvalidArgument(format!(
            "{what} {intake_id} has no corpus nodes"
        )));
    }
    Ok(nodes)
}

fn span_ref(start: (i64, i64), end: (i64, i64)) -> SpanRef {
    SpanRef {
        start_node_id: start.0,
        start_char_offset: start.1,
        end_node_id: end.0,
        end_char_offset: end.1,
    }
}

fn witness_ptrs(
    translate: &Translate,
    unit_id: i64,
) -> bookrack_translate::TranslateResult<Vec<WitnessPtr>> {
    Ok(translate
        .witnesses_for_unit(unit_id)?
        .into_iter()
        .map(|w| WitnessPtr {
            witness_intake_id: w.witness_intake_id,
            witness_node_id: w.witness_node_id,
            lang: w.lang,
            role: w.role,
            note: w.note,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// translate.plan
// ---------------------------------------------------------------------------

/// Plan one book's translation into one language: a unit per
/// leaf-bearing organizing node and a segment per leaf, cut at sentence
/// boundaries past the threshold. Idempotent: an existing unit is
/// reported with `existed`, a leaf any segment already starts or ends
/// on comes back as those segments, also `existed`, and nothing is
/// re-sliced.
pub(crate) fn plan_logic(
    translate: &Translate,
    corpus: &Corpus,
    ctx: &WriteContext<'_>,
    args: &TranslatePlanArgs,
) -> ToolResult<TranslatePlanResult> {
    if args.target_lang.trim().is_empty() {
        return Err(TranslateToolError::InvalidArgument(
            "`target_lang` is empty".to_owned(),
        ));
    }
    let profile = args.injection_profile.as_deref().unwrap_or("default");
    bookrack_translate::injection::ensure_known_profile(profile)?;
    let witness_decls = args.witnesses.as_deref().unwrap_or_default();
    for decl in witness_decls {
        if decl.lang.trim().is_empty() {
            return Err(TranslateToolError::InvalidArgument(format!(
                "witness {} has an empty `lang`",
                decl.intake_id
            )));
        }
        if !bookrack_translate::witnesses::ROLES.contains(&decl.role.as_str()) {
            return Err(TranslateError::UnknownValue {
                what: "role",
                value: decl.role.clone(),
                known: bookrack_translate::witnesses::ROLES,
            }
            .into());
        }
    }

    let nodes = book_nodes(corpus, args.intake_id, "intake")?;
    let tree = Tree::build(&nodes);
    let planned = tree.leaf_bearing(&nodes, args.chapter_node_id)?;

    // Witness alignment is resolved against the corpus before the
    // transaction opens, so a bad declaration refuses the whole plan.
    let mut witness_books = Vec::new();
    for decl in witness_decls {
        let wnodes = book_nodes(corpus, decl.intake_id, "witness intake")?;
        let wtree = Tree::build(&wnodes);
        let map: HashMap<i64, i64> = match &decl.chapter_map {
            Some(pairs) => {
                for (_, witness_node) in pairs {
                    if !wtree.by_id.contains_key(witness_node) {
                        return Err(TranslateToolError::InvalidArgument(format!(
                            "witness node {witness_node} is not a node of witness intake {}",
                            decl.intake_id
                        )));
                    }
                }
                pairs.iter().copied().collect()
            }
            None => planned
                .iter()
                .zip(wtree.leaf_bearing(&wnodes, None)?)
                .map(|((unit_node, _), (witness_node, _))| {
                    (unit_node.node_id.get(), witness_node.node_id.get())
                })
                .collect(),
        };
        witness_books.push((decl, map));
    }

    Ok(translate.transaction(|translate| {
        let mut units = Vec::new();
        let mut segments = Vec::new();
        let mut skipped = Vec::new();
        let mut created_units = 0u64;
        let mut created_segments = 0u64;

        for (unit_node, leaves) in &planned {
            let outline = tree.outline(unit_node);
            let unit_order = leaves[0].toc_lo.unwrap_or(unit_node.ordinal);
            let (unit_id, inserted) = translate.ensure_unit(&NewUnit {
                intake_id: args.intake_id,
                target_lang: &args.target_lang,
                node_id: unit_node.node_id.get(),
                unit_order,
                source_outline: Some(&outline),
                injection_profile: profile,
            })?;
            if inserted {
                created_units += 1;
            }
            // A leaf any existing segment starts or ends on is planned
            // already: its segments are reported as existing and it is
            // never re-sliced.
            let existing = translate.segments_in_unit(unit_id)?;
            let touched: HashSet<i64> = existing
                .iter()
                .flat_map(|s| [s.start_node_id, s.end_node_id])
                .collect();

            for leaf in leaves {
                let node_id = leaf.node_id.get();
                let node_type = leaf.node_type.as_str().to_owned();
                let whole = match triage(leaf.node_type) {
                    Triage::Exclude(_) => continue,
                    Triage::Skip {
                        reason,
                        needs_attention,
                    } => {
                        skipped.push(SkipReason {
                            node_id,
                            node_type,
                            reason: reason.to_owned(),
                            needs_attention,
                        });
                        continue;
                    }
                    Triage::Segment => false,
                    Triage::SegmentWhole => true,
                };
                if touched.contains(&node_id) {
                    for row in existing.iter().filter(|s| s.start_node_id == node_id) {
                        segments.push(SegmentStub {
                            segment_id: row.segment_id,
                            unit_id,
                            span: span_ref(
                                (row.start_node_id, row.start_char_offset),
                                (row.end_node_id, row.end_char_offset),
                            ),
                            char_count: row.end_char_offset - row.start_char_offset,
                            existed: true,
                        });
                    }
                    continue;
                }
                let text = leaf.text_content.as_deref().unwrap_or("");
                let chars: Vec<char> = text.chars().collect();
                if chars.is_empty() {
                    skipped.push(SkipReason {
                        node_id,
                        node_type,
                        reason: "empty leaf".to_owned(),
                        needs_attention: false,
                    });
                    continue;
                }
                let ranges = if whole {
                    vec![(0, chars.len())]
                } else {
                    let max_chars = args
                        .max_chars
                        .unwrap_or_else(|| default_max_chars(script_of(text)));
                    split_at_sentences(text, max_chars)
                };
                for (start, end) in ranges {
                    let slice: String = chars[start..end].iter().collect();
                    let (start, end) = (start as i64, end as i64);
                    let (segment_id, inserted) = translate.insert_segment(&NewSegment {
                        unit_id,
                        start_node_id: node_id,
                        start_char_offset: start,
                        end_node_id: node_id,
                        end_char_offset: end,
                        source_text_sha: span_sha256_hex(&slice),
                    })?;
                    if inserted {
                        created_segments += 1;
                    }
                    segments.push(SegmentStub {
                        segment_id,
                        unit_id,
                        span: span_ref((node_id, start), (node_id, end)),
                        char_count: end - start,
                        existed: !inserted,
                    });
                }
            }

            for (decl, map) in &witness_books {
                match map.get(&unit_node.node_id.get()) {
                    Some(witness_node) => {
                        translate.put_witness(
                            unit_id,
                            &NewWitness {
                                witness_intake_id: decl.intake_id,
                                witness_node_id: *witness_node,
                                lang: &decl.lang,
                                role: &decl.role,
                                note: decl.note.as_deref(),
                            },
                        )?;
                    }
                    None => skipped.push(SkipReason {
                        node_id: unit_node.node_id.get(),
                        node_type: unit_node.node_type.as_str().to_owned(),
                        reason: format!(
                            "no node of witness intake {} aligns with this unit",
                            decl.intake_id
                        ),
                        needs_attention: true,
                    }),
                }
            }

            units.push(UnitStub {
                unit_id,
                node_id: unit_node.node_id.get(),
                unit_order,
                source_outline: outline,
                injection_profile: profile.to_owned(),
                existed: !inserted,
                witnesses: witness_ptrs(translate, unit_id)?,
            });
        }

        let payload = serde_json::json!({
            "args": serde_json::to_value(args).unwrap_or(serde_json::Value::Null),
            "created_units": created_units,
            "created_segments": created_segments,
            "skipped": skipped.len(),
        });
        let plan_id = translate.append_audit(&ctx.audit(
            AuditSubject::None,
            "plan",
            args.reason.as_deref(),
            &payload,
            None,
        ))?;
        Ok(TranslatePlanResult {
            intake_id: args.intake_id,
            target_lang: args.target_lang.clone(),
            units,
            segments,
            skipped,
            plan_id,
            created_units,
            created_segments,
        })
    })?)
}

// ---------------------------------------------------------------------------
// translate.resegment
// ---------------------------------------------------------------------------

/// Replace the virgin segments of one unit with `new_spans`. Segments
/// that carry work stay, and no new span may touch their leaves; every
/// endpoint must be a leaf of the unit, every span well-formed, and the
/// spans must not overlap one another.
pub(crate) fn resegment_logic(
    translate: &Translate,
    corpus: &Corpus,
    ctx: &WriteContext<'_>,
    args: &TranslateResegmentArgs,
) -> ToolResult<TranslateResegmentResult> {
    if args.new_spans.is_empty() {
        return Err(TranslateToolError::InvalidArgument(
            "`new_spans` is empty".to_owned(),
        ));
    }
    let unit = translate
        .unit(args.unit_id)?
        .ok_or(TranslateError::UnknownUnit {
            unit_id: args.unit_id,
        })?;
    let out_of_unit = |reason: &'static str| TranslateToolError::SpanOutOfUnit {
        unit_id: args.unit_id,
        reason,
    };

    // The unit's leaves: position and length of each.
    let leaves: HashMap<i64, (i64, i64)> = corpus
        .children(NodeId::new(unit.node_id))?
        .into_iter()
        .filter(|n| !n.node_type.is_organizing())
        .map(|n| {
            let len = n
                .text_content
                .as_deref()
                .map(|t| t.chars().count() as i64)
                .unwrap_or(0);
            (n.node_id.get(), (n.toc_lo.unwrap_or(n.ordinal), len))
        })
        .collect();

    let existing = translate.segments_in_unit(args.unit_id)?;
    let (virgin, kept): (Vec<_>, Vec<_>) = existing.into_iter().partition(Translate::is_virgin);
    let forbidden: HashSet<i64> = kept
        .iter()
        .flat_map(|s| [s.start_node_id, s.end_node_id])
        .collect();

    // Validate every span, then their mutual order.
    let mut ordered = Vec::new();
    for span in &args.new_spans {
        let (start_pos, start_len) = *leaves
            .get(&span.start_node_id)
            .ok_or_else(|| out_of_unit("a span starts outside the unit's leaves"))?;
        let (end_pos, end_len) = *leaves
            .get(&span.end_node_id)
            .ok_or_else(|| out_of_unit("a span ends outside the unit's leaves"))?;
        if forbidden.contains(&span.start_node_id) || forbidden.contains(&span.end_node_id) {
            return Err(TranslateError::NotVirgin {
                segment_id: kept
                    .iter()
                    .find(|s| {
                        s.start_node_id == span.start_node_id
                            || s.end_node_id == span.start_node_id
                            || s.start_node_id == span.end_node_id
                            || s.end_node_id == span.end_node_id
                    })
                    .map(|s| s.segment_id)
                    .unwrap_or(0),
            }
            .into());
        }
        if span.start_char_offset < 0 || span.start_char_offset >= start_len {
            return Err(out_of_unit("a span starts past the end of its leaf"));
        }
        if span.end_char_offset <= 0 || span.end_char_offset > end_len {
            return Err(out_of_unit("a span ends past the end of its leaf"));
        }
        let start_key = (start_pos, span.start_char_offset);
        let end_key = (end_pos, span.end_char_offset);
        if start_key >= end_key {
            return Err(out_of_unit("a span ends before it starts"));
        }
        ordered.push((start_key, end_key, span.clone()));
    }
    ordered.sort_by_key(|(start, end, _)| (*start, *end));
    if ordered.windows(2).any(|w| w[1].0 < w[0].1) {
        return Err(out_of_unit("two spans overlap"));
    }

    // Fingerprints are read before the transaction: the corpus is not
    // part of it, and a drift here is a refusal, not a partial write.
    let mut rows = Vec::new();
    for (_, _, span) in &ordered {
        let leaves_run = corpus.leaves_between(
            NodeId::new(span.start_node_id),
            NodeId::new(span.end_node_id),
            bookrack_ops::dto::MAX_SPAN_LEAVES,
        )?;
        let text = span_text(&leaves_run, span.start_char_offset, span.end_char_offset);
        rows.push((
            NewSegment {
                unit_id: args.unit_id,
                start_node_id: span.start_node_id,
                start_char_offset: span.start_char_offset,
                end_node_id: span.end_node_id,
                end_char_offset: span.end_char_offset,
                source_text_sha: span_sha256_hex(&text),
            },
            text.chars().count() as i64,
        ));
    }

    Ok(translate.transaction(|translate| {
        let removed: Vec<i64> = virgin.iter().map(|s| s.segment_id).collect();
        for id in &removed {
            translate.delete_segment(*id)?;
        }
        let mut segments = Vec::new();
        for (row, char_count) in &rows {
            let (segment_id, _) = translate.insert_segment(row)?;
            segments.push(SegmentStub {
                segment_id,
                unit_id: args.unit_id,
                span: span_ref(
                    (row.start_node_id, row.start_char_offset),
                    (row.end_node_id, row.end_char_offset),
                ),
                char_count: *char_count,
                existed: false,
            });
        }
        let payload = serde_json::json!({
            "args": serde_json::to_value(args).unwrap_or(serde_json::Value::Null),
            "removed": removed,
            "inserted": segments.iter().map(|s| s.segment_id).collect::<Vec<_>>(),
        });
        let audit_id = translate.append_audit(&ctx.audit(
            AuditSubject::None,
            "resegment",
            args.reason.as_deref(),
            &payload,
            None,
        ))?;
        Ok(TranslateResegmentResult {
            unit_id: args.unit_id,
            removed,
            segments,
            audit_id,
        })
    })?)
}

// ---------------------------------------------------------------------------
// translate.propose / translate.seal / translate.import
// ---------------------------------------------------------------------------

/// The stage names `translate.propose` accepts.
pub const STAGES: &[&str] = &["draft", "reflection", "final"];

/// The one actor kind a seal may claim on behalf of someone else.
const ACTOR_OVERRIDES: &[&str] = &["human"];

/// Arguments for `translate.propose`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TranslateProposeArgs {
    /// The library to write. Required.
    pub library: String,
    pub segment_id: i64,
    /// The `current_version` the caller fetched; the write is refused
    /// when the row has moved on.
    pub expected_version: i64,
    /// `draft`, `reflection` or `final`.
    pub stage: String,
    /// The draft or final text; a revised draft at the reflection stage.
    pub text: Option<String>,
    /// The reflection: free notes or a review-note JSON array.
    pub notes: Option<String>,
    /// Why this text, recorded on the audit row. Required.
    pub rationale: String,
    /// Tokens this stage cost, for the budget sum.
    pub cost_tokens: Option<i64>,
}

/// Arguments for `translate.seal`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TranslateSealArgs {
    /// The library to write. Required.
    pub library: String,
    pub segment_id: i64,
    pub expected_version: i64,
    /// `human`, `llm-draft`, `llm-reflected`, `edited` or `imported`;
    /// derived from the segment's history when absent.
    pub source_kind: Option<String>,
    /// `human` when a person approved the text; the audit row is then
    /// attributed to them.
    pub actor_kind_override: Option<String>,
    pub rationale: Option<String>,
    pub cost_tokens: Option<i64>,
}

/// Arguments for `translate.import`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TranslateImportArgs {
    /// The library to write. Required.
    pub library: String,
    pub unit_id: i64,
    pub fills: Vec<ImportFill>,
    /// Always `imported`; stated so the caller says what it is doing.
    pub source_kind: String,
    pub reason: Option<String>,
}

/// One existing translation, aligned by the caller to one segment.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ImportFill {
    pub segment_id: i64,
    pub text: String,
}

/// What a segment write left behind.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SegmentReceipt {
    pub segment_id: i64,
    pub unit_id: i64,
    pub status: String,
    pub version: i64,
    pub source_kind: Option<String>,
    pub sealed_at: Option<String>,
    /// The audit row the write appended.
    pub audit_id: i64,
}

impl SegmentReceipt {
    fn from_row(row: &SegmentRow, audit_id: i64) -> SegmentReceipt {
        SegmentReceipt {
            segment_id: row.segment_id,
            unit_id: row.unit_id,
            status: row.status.clone(),
            version: row.version,
            source_kind: row.source_kind.clone(),
            sealed_at: row.sealed_at.clone(),
            audit_id,
        }
    }
}

/// Reply of `translate.import`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TranslateImportResult {
    pub unit_id: i64,
    pub filled: Vec<SegmentReceipt>,
    pub audit_id: i64,
}

fn require_text(what: &str, value: &str) -> ToolResult<()> {
    if value.trim().is_empty() {
        return Err(TranslateToolError::InvalidArgument(format!(
            "`{what}` is empty"
        )));
    }
    Ok(())
}

fn args_payload<T: Serialize>(args: &T) -> serde_json::Value {
    serde_json::json!({ "args": serde_json::to_value(args).unwrap_or(serde_json::Value::Null) })
}

/// Record one stage of a proposal on a segment.
pub(crate) fn propose_logic(
    translate: &Translate,
    ctx: &WriteContext<'_>,
    args: &TranslateProposeArgs,
) -> ToolResult<SegmentReceipt> {
    require_text("rationale", &args.rationale)?;
    let stage = match args.stage.as_str() {
        "draft" => ProposeStage::Draft,
        "reflection" => ProposeStage::Reflection,
        "final" => ProposeStage::Final,
        other => {
            return Err(TranslateError::UnknownValue {
                what: "stage",
                value: other.to_owned(),
                known: STAGES,
            }
            .into());
        }
    };
    if stage == ProposeStage::Reflection
        && let Some(notes) = args.notes.as_deref()
        && serde_json::from_str::<serde_json::Value>(notes).is_err()
    {
        return Err(TranslateToolError::InvalidArgument(
            "`notes` is not valid JSON".to_owned(),
        ));
    }
    let payload = args_payload(args);
    Ok(translate.transaction(|translate| {
        let row = translate.apply_propose(
            args.segment_id,
            args.expected_version,
            stage,
            args.text.as_deref(),
            args.notes.as_deref(),
        )?;
        let audit_id = translate.append_audit(&ctx.audit(
            AuditSubject::Segment(row.segment_id),
            stage.action(),
            Some(&args.rationale),
            &payload,
            args.cost_tokens,
        ))?;
        Ok(SegmentReceipt::from_row(&row, audit_id))
    })?)
}

/// Where the translation-memory index will take a sealed segment. The
/// index arrives with a later milestone; until then a seal emits
/// nothing beyond its row.
fn emit_tm_row(_sealed: &SegmentRow) {}

/// Lock a proposed segment's final text.
pub(crate) fn seal_logic(
    translate: &Translate,
    ctx: &WriteContext<'_>,
    args: &TranslateSealArgs,
) -> ToolResult<SegmentReceipt> {
    let actor_kind = match args.actor_kind_override.as_deref() {
        None => ctx.caller.actor_kind,
        Some("human") => ActorKind::Human,
        Some(other) => {
            return Err(TranslateError::UnknownValue {
                what: "actor_kind_override",
                value: other.to_owned(),
                known: ACTOR_OVERRIDES,
            }
            .into());
        }
    };
    let row = translate
        .segment(args.segment_id)?
        .ok_or(TranslateError::UnknownSegment {
            segment_id: args.segment_id,
        })?;
    // Derivation, when the caller does not say: a person's approval is
    // `human`; a reviewed import is `edited`; a drafted text is
    // `llm-reflected` when a reflection was recorded, `llm-draft`
    // otherwise.
    let source_kind = match args.source_kind.as_deref() {
        Some(kind) => kind,
        None if actor_kind == ActorKind::Human => "human",
        None if row.task_mode() == "review" => "edited",
        None if row.reflection_notes.is_some() => "llm-reflected",
        None => "llm-draft",
    };
    let payload = args_payload(args);
    Ok(translate.transaction(|translate| {
        let sealed =
            translate.seal_segment(args.segment_id, args.expected_version, source_kind, ctx.now)?;
        emit_tm_row(&sealed);
        let mut audit = ctx.audit(
            AuditSubject::Segment(sealed.segment_id),
            "seal",
            args.rationale.as_deref(),
            &payload,
            args.cost_tokens,
        );
        audit.actor_kind = actor_kind;
        let audit_id = translate.append_audit(&audit)?;
        Ok(SegmentReceipt::from_row(&sealed, audit_id))
    })?)
}

/// Fill the empty draft segments of one unit with an existing
/// translation, all or nothing.
pub(crate) fn import_logic(
    translate: &Translate,
    ctx: &WriteContext<'_>,
    args: &TranslateImportArgs,
) -> ToolResult<TranslateImportResult> {
    if args.source_kind != SOURCE_KIND_IMPORTED {
        return Err(TranslateError::UnknownValue {
            what: "source_kind",
            value: args.source_kind.clone(),
            known: &[SOURCE_KIND_IMPORTED],
        }
        .into());
    }
    if args.fills.is_empty() {
        return Err(TranslateToolError::InvalidArgument(
            "`fills` is empty".to_owned(),
        ));
    }
    if translate.unit(args.unit_id)?.is_none() {
        return Err(TranslateError::UnknownUnit {
            unit_id: args.unit_id,
        }
        .into());
    }
    for fill in &args.fills {
        if fill.text.trim().is_empty() {
            return Err(TranslateToolError::InvalidArgument(format!(
                "the fill for segment {} has empty text",
                fill.segment_id
            )));
        }
        let row = translate
            .segment(fill.segment_id)?
            .ok_or(TranslateError::UnknownSegment {
                segment_id: fill.segment_id,
            })?;
        if row.unit_id != args.unit_id {
            return Err(TranslateToolError::InvalidArgument(format!(
                "segment {} belongs to unit {}, not unit {}",
                fill.segment_id, row.unit_id, args.unit_id
            )));
        }
    }
    let payload = serde_json::json!({
        "args": serde_json::to_value(args).unwrap_or(serde_json::Value::Null),
        "segments": args.fills.iter().map(|f| f.segment_id).collect::<Vec<_>>(),
    });
    Ok(translate.transaction(|translate| {
        let mut rows = Vec::new();
        for fill in &args.fills {
            rows.push(translate.import_fill(fill.segment_id, &fill.text)?);
        }
        let audit_id = translate.append_audit(&ctx.audit(
            AuditSubject::None,
            "import",
            args.reason.as_deref(),
            &payload,
            None,
        ))?;
        Ok(TranslateImportResult {
            unit_id: args.unit_id,
            filled: rows
                .iter()
                .map(|row| SegmentReceipt::from_row(row, audit_id))
                .collect(),
            audit_id,
        })
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_corpus::{NewNode, NodeType};

    const LONG: &str = "Erster Satz. Zweiter Satz. Dritter Satz.";
    const POEM: &str = "Zeile eins. Zeile zwei.";

    /// Book 1: a chapter with two direct leaves and a figure, a section
    /// with a long paragraph, a table and a poem, and a section with two
    /// short paragraphs. Returns `[chapter, section_a, section_b]` and
    /// the leaf ids in document order.
    fn seed_book(corpus: &mut Corpus) -> (Vec<NodeId>, Vec<NodeId>) {
        let partition = corpus.allocate_partition(1).expect("partition");
        let root = partition.book_root_id;
        corpus
            .insert_node(
                &NewNode::root(root, NodeType::Work)
                    .title("A Book")
                    .toc_span(0, 7),
            )
            .expect("root");
        let ids = corpus.allocate_node_ids(partition.idx, 11).expect("ids");
        let (chapter, section_a, section_b) = (ids[0], ids[1], ids[2]);
        let leaf = |i: usize| ids[3 + i];
        let insert = |node: NewNode| corpus.insert_node(&node).expect("node");
        insert(
            NewNode::child(chapter, root, root, 0, 1, NodeType::Chapter)
                .title("I")
                .toc_span(0, 7),
        );
        insert(
            NewNode::child(leaf(0), chapter, root, 0, 2, NodeType::Paragraph)
                .text("Gregor Samsa erwachte.")
                .toc_span(0, 0),
        );
        insert(
            NewNode::child(leaf(1), chapter, root, 1, 2, NodeType::Code)
                .text("x = 1")
                .toc_span(1, 1),
        );
        insert(
            NewNode::child(section_a, chapter, root, 2, 2, NodeType::Section)
                .title("I.1")
                .toc_span(2, 4),
        );
        insert(
            NewNode::child(leaf(2), section_a, root, 0, 3, NodeType::Paragraph)
                .text(LONG)
                .toc_span(2, 2),
        );
        insert(
            NewNode::child(leaf(3), section_a, root, 1, 3, NodeType::Table)
                .text("a | b")
                .toc_span(3, 3),
        );
        insert(
            NewNode::child(leaf(4), section_a, root, 2, 3, NodeType::Poem)
                .text(POEM)
                .toc_span(4, 4),
        );
        insert(
            NewNode::child(section_b, chapter, root, 3, 2, NodeType::Section)
                .title("I.2")
                .toc_span(5, 6),
        );
        insert(
            NewNode::child(leaf(5), section_b, root, 0, 3, NodeType::Paragraph)
                .text("Fuenf.")
                .toc_span(5, 5),
        );
        insert(
            NewNode::child(leaf(6), section_b, root, 1, 3, NodeType::Paragraph)
                .text("Sechs.")
                .toc_span(6, 6),
        );
        insert(NewNode::child(leaf(7), chapter, root, 4, 2, NodeType::Figure).toc_span(7, 7));
        (
            vec![chapter, section_a, section_b],
            (0..8).map(leaf).collect(),
        )
    }

    /// Book 2: a chapter holding `sections` sections of one paragraph
    /// each. Returns the section ids.
    fn seed_witness(corpus: &mut Corpus, sections: usize) -> Vec<NodeId> {
        let partition = corpus.allocate_partition(2).expect("partition");
        let root = partition.book_root_id;
        corpus
            .insert_node(
                &NewNode::root(root, NodeType::Work)
                    .title("Witness")
                    .toc_span(0, 10),
            )
            .expect("root");
        let ids = corpus
            .allocate_node_ids(
                partition.idx,
                u32::try_from(1 + 2 * sections).expect("small"),
            )
            .expect("ids");
        let chapter = ids[0];
        corpus
            .insert_node(
                &NewNode::child(chapter, root, root, 0, 1, NodeType::Chapter)
                    .title("One")
                    .toc_span(0, 10),
            )
            .expect("chapter");
        let mut out = Vec::new();
        for i in 0..sections {
            let (section, para) = (ids[1 + 2 * i], ids[2 + 2 * i]);
            let pos = i64::try_from(i).expect("small");
            corpus
                .insert_node(
                    &NewNode::child(section, chapter, root, pos, 2, NodeType::Section)
                        .title(format!("W{i}"))
                        .toc_span(pos, pos),
                )
                .expect("section");
            corpus
                .insert_node(
                    &NewNode::child(para, section, root, 0, 3, NodeType::Paragraph)
                        .text("Witness text.")
                        .toc_span(pos, pos),
                )
                .expect("para");
            out.push(section);
        }
        out
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        translate_db: std::path::PathBuf,
        corpus: Corpus,
        units: Vec<NodeId>,
        leaves: Vec<NodeId>,
        caller: Caller,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut corpus = Corpus::open_in_memory().expect("corpus");
            let (units, leaves) = seed_book(&mut corpus);
            Fixture {
                translate_db: dir.path().join("translate.db"),
                _dir: dir,
                corpus,
                units,
                leaves,
                caller: Caller::mcp(),
            }
        }

        fn ctx(&self) -> WriteContext<'_> {
            WriteContext {
                caller: &self.caller,
                now: "2026-01-01T00:00:00Z",
            }
        }

        fn store(&self) -> Translate {
            Translate::open(&self.translate_db).expect("open")
        }

        fn plan_args(&self) -> TranslatePlanArgs {
            TranslatePlanArgs {
                library: "lab".into(),
                intake_id: 1,
                target_lang: "zh".into(),
                chapter_node_id: None,
                max_chars: None,
                injection_profile: None,
                witnesses: None,
                reason: Some("first plan".into()),
            }
        }

        fn plan(&self, args: &TranslatePlanArgs) -> ToolResult<TranslatePlanResult> {
            let translate = self.store();
            plan_logic(&translate, &self.corpus, &self.ctx(), args)
        }
    }

    fn ids(stubs: &[SegmentStub]) -> Vec<i64> {
        stubs.iter().map(|s| s.segment_id).collect()
    }

    #[test]
    fn a_whole_book_plan_makes_one_unit_per_leaf_bearing_node_in_document_order() {
        let fx = Fixture::new();
        assert!(!fx.translate_db.exists());
        let result = fx.plan(&fx.plan_args()).expect("plan");
        assert!(
            fx.translate_db.exists(),
            "the plan is the write that creates the store"
        );

        let nodes: Vec<i64> = result.units.iter().map(|u| u.node_id).collect();
        assert_eq!(nodes, fx.units.iter().map(|n| n.get()).collect::<Vec<_>>());
        let orders: Vec<i64> = result.units.iter().map(|u| u.unit_order).collect();
        assert_eq!(orders, vec![0, 2, 5]);
        assert_eq!(result.units[1].source_outline, "A Book > I > I.1");
        assert!(
            result
                .units
                .iter()
                .all(|u| !u.existed && u.injection_profile == "default")
        );
        assert_eq!((result.created_units, result.created_segments), (3, 5));

        // One segment per translatable leaf, whole leaves at the default
        // threshold; the long paragraph stays one segment.
        let leaf_of: Vec<i64> = result
            .segments
            .iter()
            .map(|s| s.span.start_node_id)
            .collect();
        assert_eq!(
            leaf_of,
            [0usize, 2, 4, 5, 6]
                .iter()
                .map(|&i| fx.leaves[i].get())
                .collect::<Vec<_>>()
        );
        assert!(result.segments.iter().all(|s| !s.existed));
        let long = &result.segments[1];
        assert_eq!(
            (
                long.span.start_char_offset,
                long.span.end_char_offset,
                long.char_count
            ),
            (0, 40, 40)
        );

        // Code and the table are skipped and say why; the figure is not
        // reported at all.
        let skipped: Vec<(i64, &str, bool)> = result
            .skipped
            .iter()
            .map(|s| (s.node_id, s.node_type.as_str(), s.needs_attention))
            .collect();
        assert_eq!(
            skipped,
            vec![
                (fx.leaves[1].get(), "code", false),
                (fx.leaves[3].get(), "table", true)
            ]
        );

        // The fingerprints match what the read side will compute.
        let translate = fx.store();
        for stub in &result.segments {
            let row = translate
                .segment(stub.segment_id)
                .expect("read")
                .expect("row");
            let leaf = fx
                .corpus
                .get_node(NodeId::new(stub.span.start_node_id))
                .expect("node")
                .expect("leaf");
            let text: String = leaf
                .text_content
                .unwrap_or_default()
                .chars()
                .skip(stub.span.start_char_offset as usize)
                .take(stub.char_count as usize)
                .collect();
            assert_eq!(row.source_text_sha, span_sha256_hex(&text));
        }

        // One audit row, attributed to the MCP caller.
        let conn = rusqlite::Connection::open(&fx.translate_db).expect("plain connection");
        let plan_row: (String, String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT action, actor_kind, actor_detail, reason FROM translate_audit WHERE audit_id = ?1",
                [result.plan_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("audit");
        assert_eq!(
            plan_row,
            (
                "plan".into(),
                "llm".into(),
                Some("mcp".into()),
                Some("first plan".into())
            )
        );
    }

    #[test]
    fn replanning_creates_nothing_and_reports_everything_as_existing() {
        let fx = Fixture::new();
        let first = fx.plan(&fx.plan_args()).expect("plan");
        let again = fx.plan(&fx.plan_args()).expect("replan");
        assert_eq!((again.created_units, again.created_segments), (0, 0));
        assert!(again.units.iter().all(|u| u.existed));
        assert_eq!(ids(&again.segments), ids(&first.segments));
        assert!(again.segments.iter().all(|s| s.existed));
        assert_eq!(
            again.skipped, first.skipped,
            "a replan does not report planned leaves as skipped"
        );
        assert_ne!(
            again.plan_id, first.plan_id,
            "every plan call leaves its own audit row"
        );
    }

    #[test]
    fn a_threshold_cuts_a_long_paragraph_at_sentences_but_never_a_poem() {
        let fx = Fixture::new();
        let args = TranslatePlanArgs {
            max_chars: Some(15),
            chapter_node_id: Some(fx.units[1].get()),
            ..fx.plan_args()
        };
        let result = fx.plan(&args).expect("plan");
        assert_eq!(
            result.units.len(),
            1,
            "the chapter scope narrows to one section"
        );
        let long_leaf = fx.leaves[2].get();
        let cuts: Vec<(i64, i64)> = result
            .segments
            .iter()
            .filter(|s| s.span.start_node_id == long_leaf)
            .map(|s| (s.span.start_char_offset, s.span.end_char_offset))
            .collect();
        assert_eq!(cuts, vec![(0, 13), (13, 27), (27, 40)]);
        let poem: Vec<(i64, i64)> = result
            .segments
            .iter()
            .filter(|s| s.span.start_node_id == fx.leaves[4].get())
            .map(|s| (s.span.start_char_offset, s.span.end_char_offset))
            .collect();
        assert_eq!(poem, vec![(0, POEM.chars().count() as i64)]);

        // Re-planning the same leaf at the default threshold does not
        // re-slice it: the three cuts come back as existing.
        let whole = fx
            .plan(&TranslatePlanArgs {
                max_chars: None,
                ..args
            })
            .expect("replan");
        assert_eq!(whole.created_segments, 0);
        let again: Vec<(i64, i64, bool)> = whole
            .segments
            .iter()
            .filter(|s| s.span.start_node_id == long_leaf)
            .map(|s| (s.span.start_char_offset, s.span.end_char_offset, s.existed))
            .collect();
        assert_eq!(again, vec![(0, 13, true), (13, 27, true), (27, 40, true)]);
    }

    #[test]
    fn a_chapter_scope_outside_the_book_or_on_a_leaf_is_refused() {
        let fx = Fixture::new();
        let err = fx
            .plan(&TranslatePlanArgs {
                chapter_node_id: Some(fx.leaves[0].get()),
                ..fx.plan_args()
            })
            .expect_err("leaf");
        assert!(
            matches!(err, TranslateToolError::InvalidArgument(ref m) if m.contains("is a leaf")),
            "{err:?}"
        );
        let err = fx
            .plan(&TranslatePlanArgs {
                chapter_node_id: Some(999_999),
                ..fx.plan_args()
            })
            .expect_err("foreign");
        assert!(
            matches!(err, TranslateToolError::InvalidArgument(_)),
            "{err:?}"
        );
        let err = fx
            .plan(&TranslatePlanArgs {
                intake_id: 9,
                ..fx.plan_args()
            })
            .expect_err("no nodes");
        assert!(
            matches!(err, TranslateToolError::InvalidArgument(ref m) if m.contains("no corpus nodes")),
            "{err:?}"
        );
        let err = fx
            .plan(&TranslatePlanArgs {
                injection_profile: Some("verbose".into()),
                ..fx.plan_args()
            })
            .expect_err("profile");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::UnknownProfile { .. })
            ),
            "{err:?}"
        );
        let translate = fx.store();
        assert!(
            translate.units_for(1, "zh").expect("read").is_empty(),
            "a refused plan writes nothing"
        );
    }

    #[test]
    fn witnesses_align_by_document_order_and_an_unaligned_unit_is_reported() {
        let mut fx = Fixture::new();
        let sections = seed_witness(&mut fx.corpus, 2);
        let args = TranslatePlanArgs {
            witnesses: Some(vec![WitnessDecl {
                intake_id: 2,
                lang: "en".into(),
                role: "translation_witness".into(),
                note: Some("1999".into()),
                chapter_map: None,
            }]),
            ..fx.plan_args()
        };
        let result = fx.plan(&args).expect("plan");
        let anchored: Vec<Vec<i64>> = result
            .units
            .iter()
            .map(|u| u.witnesses.iter().map(|w| w.witness_node_id).collect())
            .collect();
        assert_eq!(
            anchored,
            vec![vec![sections[0].get()], vec![sections[1].get()], vec![]]
        );
        assert_eq!(result.units[0].witnesses[0].note.as_deref(), Some("1999"));
        let unaligned: Vec<i64> = result
            .skipped
            .iter()
            .filter(|s| s.reason.contains("aligns"))
            .map(|s| s.node_id)
            .collect();
        assert_eq!(unaligned, vec![fx.units[2].get()]);

        // An explicit map overrides the order and replaces the row.
        let mapped = TranslatePlanArgs {
            witnesses: Some(vec![WitnessDecl {
                chapter_map: Some(vec![(fx.units[2].get(), sections[0].get())]),
                ..args.witnesses.clone().unwrap().remove(0)
            }]),
            ..args.clone()
        };
        let result = fx.plan(&mapped).expect("replan");
        assert_eq!(
            result.units[2].witnesses[0].witness_node_id,
            sections[0].get()
        );
        assert_eq!(
            result.units[0].witnesses[0].witness_node_id,
            sections[0].get(),
            "earlier rows stay"
        );

        let err = fx
            .plan(&TranslatePlanArgs {
                witnesses: Some(vec![WitnessDecl {
                    role: "bystander".into(),
                    ..args.witnesses.clone().unwrap().remove(0)
                }]),
                ..args.clone()
            })
            .expect_err("role");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::UnknownValue { what: "role", .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn resegment_merges_virgin_leaves_and_leaves_worked_segments_alone() {
        let fx = Fixture::new();
        let planned = fx.plan(&fx.plan_args()).expect("plan");
        let unit_b = planned.units[2].unit_id;
        let (fuenf, sechs) = (fx.leaves[5].get(), fx.leaves[6].get());
        let translate = fx.store();

        let result = resegment_logic(
            &translate,
            &fx.corpus,
            &fx.ctx(),
            &TranslateResegmentArgs {
                library: "lab".into(),
                unit_id: unit_b,
                new_spans: vec![span_ref((fuenf, 0), (sechs, 6))],
                reason: None,
            },
        )
        .expect("merge");
        let before: Vec<i64> = planned
            .segments
            .iter()
            .filter(|s| s.unit_id == unit_b)
            .map(|s| s.segment_id)
            .collect();
        assert_eq!(result.removed, before);
        assert_eq!(result.segments.len(), 1);
        let merged = translate
            .segment(result.segments[0].segment_id)
            .expect("read")
            .expect("row");
        assert_eq!(
            (
                merged.start_node_id,
                merged.end_node_id,
                merged.end_char_offset
            ),
            (fuenf, sechs, 6)
        );
        assert_eq!(merged.source_text_sha, span_sha256_hex("Fuenf.\n\nSechs."));
        assert_eq!(result.segments[0].char_count, 14);
        assert_eq!(translate.segments_in_unit(unit_b).expect("read").len(), 1);

        // Work on the merged segment makes it untouchable.
        translate
            .apply_propose(merged.segment_id, 1, ProposeStage::Draft, Some("x"), None)
            .expect("draft");
        let err = resegment_logic(
            &translate,
            &fx.corpus,
            &fx.ctx(),
            &TranslateResegmentArgs {
                library: "lab".into(),
                unit_id: unit_b,
                new_spans: vec![span_ref((fuenf, 0), (fuenf, 6))],
                reason: None,
            },
        )
        .expect_err("not virgin");
        assert!(
            matches!(err, TranslateToolError::Translate(TranslateError::NotVirgin { segment_id }) if segment_id == merged.segment_id),
            "{err:?}"
        );
    }

    #[test]
    fn resegment_refuses_spans_outside_the_unit_overlaps_and_unknown_units() {
        let fx = Fixture::new();
        let planned = fx.plan(&fx.plan_args()).expect("plan");
        let unit_b = planned.units[2].unit_id;
        let (fuenf, sechs, foreign) = (fx.leaves[5].get(), fx.leaves[6].get(), fx.leaves[0].get());
        let translate = fx.store();
        let attempt = |spans: Vec<SpanRef>| {
            resegment_logic(
                &translate,
                &fx.corpus,
                &fx.ctx(),
                &TranslateResegmentArgs {
                    library: "lab".into(),
                    unit_id: unit_b,
                    new_spans: spans,
                    reason: None,
                },
            )
        };
        let cases: Vec<(Vec<SpanRef>, &str)> = vec![
            (vec![span_ref((foreign, 0), (fuenf, 6))], "outside"),
            (vec![span_ref((fuenf, 0), (fuenf, 9))], "past the end"),
            (
                vec![span_ref((sechs, 0), (fuenf, 6))],
                "ends before it starts",
            ),
            (
                vec![
                    span_ref((fuenf, 0), (fuenf, 4)),
                    span_ref((fuenf, 3), (sechs, 6)),
                ],
                "overlap",
            ),
        ];
        for (spans, what) in cases {
            let err = attempt(spans).expect_err(what);
            assert!(
                matches!(err, TranslateToolError::SpanOutOfUnit { .. }),
                "{what}: {err:?}"
            );
        }
        assert_eq!(
            translate.segments_in_unit(unit_b).expect("read").len(),
            2,
            "a refusal changes nothing"
        );
        let err = attempt(vec![]).expect_err("empty");
        assert!(
            matches!(err, TranslateToolError::InvalidArgument(_)),
            "{err:?}"
        );
        let err = resegment_logic(
            &translate,
            &fx.corpus,
            &fx.ctx(),
            &TranslateResegmentArgs {
                library: "lab".into(),
                unit_id: 404,
                new_spans: vec![span_ref((fuenf, 0), (fuenf, 6))],
                reason: None,
            },
        )
        .expect_err("unit");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::UnknownUnit { unit_id: 404 })
            ),
            "{err:?}"
        );
    }

    fn propose_args(segment_id: i64, version: i64, stage: &str) -> TranslateProposeArgs {
        TranslateProposeArgs {
            library: "lab".into(),
            segment_id,
            expected_version: version,
            stage: stage.into(),
            text: Some(format!("{stage} text")),
            notes: None,
            rationale: "because".into(),
            cost_tokens: Some(10),
        }
    }

    fn seal_args(segment_id: i64, version: i64) -> TranslateSealArgs {
        TranslateSealArgs {
            library: "lab".into(),
            segment_id,
            expected_version: version,
            source_kind: None,
            actor_kind_override: None,
            rationale: None,
            cost_tokens: Some(3),
        }
    }

    fn audit_rows(translate: &Translate, segment_id: i64) -> Vec<(String, String, Option<i64>)> {
        translate
            .audit_for_segment(segment_id)
            .expect("audit")
            .into_iter()
            .map(|r| (r.action, r.actor_kind, r.cost_tokens))
            .collect()
    }

    #[test]
    fn a_proposal_walks_three_stages_and_each_leaves_an_audit_row() {
        let fx = Fixture::new();
        let planned = fx.plan(&fx.plan_args()).expect("plan");
        let id = planned.segments[0].segment_id;
        let translate = fx.store();

        let r = propose_logic(&translate, &fx.ctx(), &propose_args(id, 1, "draft")).expect("draft");
        assert_eq!((r.status.as_str(), r.version), ("draft", 2));
        let mut reflection = propose_args(id, 2, "reflection");
        reflection.text = None;
        reflection.notes = Some("[{\"verdict\":\"pass\"}]".into());
        let r = propose_logic(&translate, &fx.ctx(), &reflection).expect("reflection");
        assert_eq!((r.status.as_str(), r.version), ("draft", 3));
        let r = propose_logic(&translate, &fx.ctx(), &propose_args(id, 3, "final")).expect("final");
        assert_eq!(
            (r.status.as_str(), r.version, r.source_kind),
            ("proposed", 4, None)
        );

        let row = translate.segment(id).expect("read").expect("row");
        assert_eq!(
            (
                row.draft_text.as_deref(),
                row.reflection_notes.as_deref(),
                row.final_text.as_deref()
            ),
            (
                Some("draft text"),
                Some("[{\"verdict\":\"pass\"}]"),
                Some("final text")
            )
        );
        assert_eq!(
            audit_rows(&translate, id),
            vec![
                ("propose_draft".into(), "llm".into(), Some(10)),
                ("propose_reflection".into(), "llm".into(), Some(10)),
                ("propose_final".into(), "llm".into(), Some(10)),
            ]
        );
        let last = translate
            .audit_for_segment(id)
            .expect("audit")
            .pop()
            .expect("row");
        assert_eq!(
            (last.actor_detail.as_deref(), last.reason.as_deref()),
            (Some("mcp"), Some("because"))
        );
        assert!(
            last.payload_json
                .as_deref()
                .is_some_and(|p| p.contains("final text"))
        );
        assert_eq!(translate.sum_cost_tokens(1, "zh").expect("sum"), 30);
    }

    #[test]
    fn a_proposal_is_refused_for_a_stale_version_an_unknown_stage_or_bad_arguments() {
        let fx = Fixture::new();
        let planned = fx.plan(&fx.plan_args()).expect("plan");
        let id = planned.segments[0].segment_id;
        let translate = fx.store();
        propose_logic(&translate, &fx.ctx(), &propose_args(id, 1, "draft")).expect("draft");

        let err =
            propose_logic(&translate, &fx.ctx(), &propose_args(id, 1, "draft")).expect_err("stale");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::VersionConflict {
                    expected: 1,
                    current: 2,
                    ..
                })
            ),
            "{err:?}"
        );
        let err = propose_logic(&translate, &fx.ctx(), &propose_args(id, 2, "polish"))
            .expect_err("stage");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::UnknownValue { what: "stage", .. })
            ),
            "{err:?}"
        );
        let mut empty = propose_args(id, 2, "draft");
        empty.rationale = "  ".into();
        let err = propose_logic(&translate, &fx.ctx(), &empty).expect_err("rationale");
        assert!(
            matches!(err, TranslateToolError::InvalidArgument(ref m) if m.contains("rationale")),
            "{err:?}"
        );
        let mut bad_notes = propose_args(id, 2, "reflection");
        bad_notes.notes = Some("not json".into());
        let err = propose_logic(&translate, &fx.ctx(), &bad_notes).expect_err("notes");
        assert!(
            matches!(err, TranslateToolError::InvalidArgument(ref m) if m.contains("JSON")),
            "{err:?}"
        );
        let err = propose_logic(&translate, &fx.ctx(), &propose_args(404, 1, "draft"))
            .expect_err("unknown");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::UnknownSegment { segment_id: 404 })
            ),
            "{err:?}"
        );

        assert_eq!(
            audit_rows(&translate, id).len(),
            1,
            "a refused write leaves no audit row"
        );
        assert_eq!(
            translate.segment(id).expect("read").expect("row").version,
            2
        );
    }

    #[test]
    fn a_seal_derives_the_source_kind_and_takes_a_human_override() {
        let fx = Fixture::new();
        let planned = fx.plan(&fx.plan_args()).expect("plan");
        let translate = fx.store();
        let [a, b, c, d, _] = planned
            .segments
            .iter()
            .map(|s| s.segment_id)
            .collect::<Vec<_>>()[..]
        else {
            panic!("five segments")
        };

        // Not yet proposed: refused, nothing recorded.
        let err = seal_logic(&translate, &fx.ctx(), &seal_args(a, 1)).expect_err("draft");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::WrongStatus { .. })
            ),
            "{err:?}"
        );

        // Draft then final, no reflection: llm-draft.
        propose_logic(&translate, &fx.ctx(), &propose_args(a, 1, "draft")).expect("draft");
        propose_logic(&translate, &fx.ctx(), &propose_args(a, 2, "final")).expect("final");
        let r = seal_logic(&translate, &fx.ctx(), &seal_args(a, 3)).expect("seal");
        assert_eq!(
            (
                r.status.as_str(),
                r.source_kind.as_deref(),
                r.sealed_at.as_deref(),
                r.version
            ),
            ("sealed", Some("llm-draft"), Some("2026-01-01T00:00:00Z"), 4)
        );
        assert_eq!(
            audit_rows(&translate, a).last().cloned(),
            Some(("seal".into(), "llm".into(), Some(3)))
        );

        // With a reflection: llm-reflected.
        propose_logic(&translate, &fx.ctx(), &propose_args(b, 1, "draft")).expect("draft");
        let mut reflection = propose_args(b, 2, "reflection");
        reflection.notes = Some("\"thought\"".into());
        propose_logic(&translate, &fx.ctx(), &reflection).expect("reflection");
        propose_logic(&translate, &fx.ctx(), &propose_args(b, 3, "final")).expect("final");
        let r = seal_logic(&translate, &fx.ctx(), &seal_args(b, 4)).expect("seal");
        assert_eq!(r.source_kind.as_deref(), Some("llm-reflected"));

        // A person approving: human, and the audit row says so.
        propose_logic(&translate, &fx.ctx(), &propose_args(c, 1, "final")).expect("final");
        let mut human = seal_args(c, 2);
        human.actor_kind_override = Some("human".into());
        human.rationale = Some("approved on review".into());
        let r = seal_logic(&translate, &fx.ctx(), &human).expect("seal");
        assert_eq!(r.source_kind.as_deref(), Some("human"));
        let last = translate
            .audit_for_segment(c)
            .expect("audit")
            .pop()
            .expect("row");
        assert_eq!(
            (
                last.actor_kind.as_str(),
                last.actor_detail.as_deref(),
                last.reason.as_deref()
            ),
            ("human", Some("mcp"), Some("approved on review"))
        );

        // An explicit kind wins; an unknown one or override is refused.
        propose_logic(&translate, &fx.ctx(), &propose_args(d, 1, "final")).expect("final");
        let mut explicit = seal_args(d, 2);
        explicit.source_kind = Some("robot".into());
        let err = seal_logic(&translate, &fx.ctx(), &explicit).expect_err("kind");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::UnknownValue {
                    what: "source_kind",
                    ..
                })
            ),
            "{err:?}"
        );
        let mut bad_override = seal_args(d, 2);
        bad_override.actor_kind_override = Some("import".into());
        let err = seal_logic(&translate, &fx.ctx(), &bad_override).expect_err("override");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::UnknownValue {
                    what: "actor_kind_override",
                    ..
                })
            ),
            "{err:?}"
        );
        explicit.source_kind = Some("edited".into());
        let r = seal_logic(&translate, &fx.ctx(), &explicit).expect("seal");
        assert_eq!(r.source_kind.as_deref(), Some("edited"));

        // The read side sees the sealed segment in the unit's history.
        let catalogs = crate::reference::catalogs().expect("catalogs");
        let package = crate::translate::fetch_segment_logic(
            Some(&translate),
            &fx.corpus,
            None,
            catalogs,
            &crate::translate::TranslateFetchSegmentArgs {
                library: None,
                segment_id: planned.segments[4].segment_id,
                injection_profile: None,
                neighbors: None,
            },
        )
        .expect("fetch")
        .expect("package");
        assert_eq!(package.current_status, "draft");
        let sealed_ids: Vec<i64> = package
            .history_in_unit
            .iter()
            .map(|p| p.segment_id)
            .collect();
        assert_eq!(
            sealed_ids,
            vec![d],
            "the other sealed segments sit in other units"
        );
    }

    #[test]
    fn an_import_fills_a_unit_all_or_nothing_and_reads_as_review_work() {
        let fx = Fixture::new();
        let planned = fx.plan(&fx.plan_args()).expect("plan");
        let unit_b = planned.units[2].unit_id;
        let in_b: Vec<i64> = planned
            .segments
            .iter()
            .filter(|s| s.unit_id == unit_b)
            .map(|s| s.segment_id)
            .collect();
        let translate = fx.store();
        let fills = |texts: &[(i64, &str)]| {
            texts
                .iter()
                .map(|(segment_id, text)| ImportFill {
                    segment_id: *segment_id,
                    text: (*text).to_owned(),
                })
                .collect::<Vec<_>>()
        };
        let args = |fills: Vec<ImportFill>, kind: &str| TranslateImportArgs {
            library: "lab".into(),
            unit_id: unit_b,
            fills,
            source_kind: kind.into(),
            reason: Some("from the 1999 edition".into()),
        };

        let err = import_logic(
            &translate,
            &fx.ctx(),
            &args(fills(&[(in_b[0], "x")]), "human"),
        )
        .expect_err("kind");
        assert!(
            matches!(
                err,
                TranslateToolError::Translate(TranslateError::UnknownValue {
                    what: "source_kind",
                    ..
                })
            ),
            "{err:?}"
        );
        let foreign = planned.segments[0].segment_id;
        let err = import_logic(
            &translate,
            &fx.ctx(),
            &args(fills(&[(foreign, "x")]), "imported"),
        )
        .expect_err("unit");
        assert!(
            matches!(err, TranslateToolError::InvalidArgument(ref m) if m.contains("belongs to unit")),
            "{err:?}"
        );

        // One segment already drafted: the whole batch is refused.
        propose_logic(&translate, &fx.ctx(), &propose_args(in_b[1], 1, "draft")).expect("draft");
        let err = import_logic(
            &translate,
            &fx.ctx(),
            &args(fills(&[(in_b[0], "five"), (in_b[1], "six")]), "imported"),
        )
        .expect_err("batch");
        assert!(
            matches!(err, TranslateToolError::Translate(TranslateError::NotEmpty { segment_id }) if segment_id == in_b[1]),
            "{err:?}"
        );
        let untouched = translate.segment(in_b[0]).expect("read").expect("row");
        assert_eq!(
            (untouched.status.as_str(), untouched.final_text),
            ("draft", None)
        );

        let result = import_logic(
            &translate,
            &fx.ctx(),
            &args(fills(&[(in_b[0], "five")]), "imported"),
        )
        .expect("import");
        assert_eq!(result.filled.len(), 1);
        let r = &result.filled[0];
        assert_eq!(
            (
                r.status.as_str(),
                r.source_kind.as_deref(),
                r.version,
                r.audit_id
            ),
            ("proposed", Some("imported"), 2, result.audit_id)
        );
        let row = translate.segment(in_b[0]).expect("read").expect("row");
        assert_eq!(
            (row.task_mode(), row.final_text.as_deref()),
            ("review", Some("five"))
        );
        let reviewed = seal_logic(&translate, &fx.ctx(), &seal_args(in_b[0], 2)).expect("seal");
        assert_eq!(
            reviewed.source_kind.as_deref(),
            Some("edited"),
            "a reviewed import seals as edited"
        );
    }
}
