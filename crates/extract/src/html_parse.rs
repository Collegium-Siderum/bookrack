// SPDX-License-Identifier: Apache-2.0

//! XHTML / HTML → ordered [`Block`]s plus an anchor index.
//!
//! EPUB content documents are structured XHTML, so this is a DOM walk,
//! not heuristic paragraph-finding. `scraper` (html5ever backend) is
//! used for its browser-grade tolerance of the malformed XHTML that
//! real-world EPUB production tools emit.
//!
//! The walk classifies *block-level* elements. An element is emitted as
//! one `Block` when it has no block-level descendant of its own;
//! otherwise the walk descends into it. A non-block container with no
//! block descendants but with text (e.g. a `<div>`-as-paragraph) is
//! also emitted, so prose is never silently dropped.
//!
//! Anchors (TOC link targets) are collected in document order, each
//! mapped to the block it falls inside or immediately precedes. This
//! catches the common case of a chapter anchor sitting on an empty
//! `<a id>` / `<a name>` between paragraphs, or on an inline element
//! inside a paragraph — neither of which is a block of its own.

use bookrack_audit_profile::HtmlToggles;
use scraper::{ElementRef, Html, Selector};

use crate::contract::{Block, BlockKind};

/// One document parsed: ordered blocks plus its anchor index.
pub struct ParsedDoc {
    pub blocks: Vec<Block>,
    /// `(anchor id, block index within `blocks`)`. An id may appear more
    /// than once; the first occurrence is the one to trust.
    pub anchors: Vec<(String, usize)>,
}

/// Inherited classification context while descending the tree.
#[derive(Clone, Copy, Default)]
struct Ctx {
    /// Inside a footnote / endnote container.
    footnote: bool,
    /// Inside a `<figure>` (so a `<figcaption>` reads as a caption).
    caption: bool,
}

/// Mutable walk state.
struct State {
    source_unit: u32,
    blocks: Vec<Block>,
    anchors: Vec<(String, usize)>,
}

/// Parse one document's XHTML. `source_unit` is the document's
/// reading-order index. `html_toggles` carries the configurable
/// block-level and skip-tag lists the DOM walk consults.
pub fn parse_blocks(xhtml: &str, source_unit: u32, html_toggles: &HtmlToggles) -> ParsedDoc {
    let doc = Html::parse_document(xhtml);
    let body_sel = Selector::parse("body").expect("static selector");
    let root = doc
        .select(&body_sel)
        .next()
        .unwrap_or_else(|| doc.root_element());

    let mut st = State {
        source_unit,
        blocks: Vec::new(),
        anchors: Vec::new(),
    };
    walk(root, Ctx::default(), &mut st, html_toggles);

    // An anchor recorded after the last block of the document points one
    // past the end; clamp it to the final block so it still resolves.
    let last = st.blocks.len().saturating_sub(1);
    if st.blocks.is_empty() {
        st.anchors.clear();
    } else {
        for (_, idx) in &mut st.anchors {
            if *idx > last {
                *idx = last;
            }
        }
    }
    ParsedDoc {
        blocks: st.blocks,
        anchors: st.anchors,
    }
}

fn walk(el: ElementRef, ctx: Ctx, st: &mut State, html_toggles: &HtmlToggles) {
    for child in el.children() {
        let Some(child) = ElementRef::wrap(child) else {
            continue;
        };
        let name = child.value().name();
        if html_toggles.skip_tags.iter().any(|t| t == name) {
            continue;
        }
        // The element's own anchor ids resolve to the next block emitted.
        let next_block = st.blocks.len();
        for id in element_ids(child) {
            st.anchors.push((id, next_block));
        }
        let child_ctx = Ctx {
            footnote: ctx.footnote || is_footnote(child),
            caption: ctx.caption || name == "figure",
        };
        if has_block_descendant(child, html_toggles) {
            walk(child, child_ctx, st, html_toggles);
        } else {
            emit(child, child_ctx, st);
        }
    }
}

/// Emit one block for a leaf element. A leaf with no text is skipped.
fn emit(el: ElementRef, ctx: Ctx, st: &mut State) {
    let text = collect_text(el);
    if text.is_empty() {
        return;
    }
    let index = st.blocks.len();
    // Anchor ids on inline descendants resolve to this block (the
    // element's own ids were already recorded by the caller).
    let self_node = el.id();
    for descendant in el.descendants().filter_map(ElementRef::wrap) {
        if descendant.id() != self_node {
            for id in element_ids(descendant) {
                st.anchors.push((id, index));
            }
        }
    }

    let name = el.value().name();
    let kind = if let Some(level) = heading_level(name) {
        BlockKind::Heading { level }
    } else if ctx.footnote || is_footnote(el) {
        BlockKind::Footnote
    } else if ctx.caption || name == "figcaption" {
        BlockKind::Caption
    } else if matches!(name, "pre" | "table" | "td" | "th") {
        BlockKind::Other
    } else {
        BlockKind::Body
    };
    st.blocks.push(Block {
        kind,
        text,
        source_unit: st.source_unit,
        style: None,
    });
}

/// The anchor ids an element exposes: its `id`, plus the legacy `name`
/// attribute of an `<a>` (EPUB 2 NCX targets often point at `<a name>`).
fn element_ids(el: ElementRef) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = el.value().id()
        && !id.is_empty()
    {
        ids.push(id.to_string());
    }
    if el.value().name() == "a"
        && let Some(name) = el.value().attr("name")
        && !name.is_empty()
    {
        ids.push(name.to_string());
    }
    ids
}

/// Whether `el` contains any block-level element other than itself.
fn has_block_descendant(el: ElementRef, html_toggles: &HtmlToggles) -> bool {
    let self_node = el.id();
    el.descendants().filter_map(ElementRef::wrap).any(|d| {
        d.id() != self_node
            && html_toggles
                .block_tags
                .iter()
                .any(|t| t == d.value().name())
    })
}

/// All descendant text, with runs of whitespace (XML formatting
/// indentation, line breaks) collapsed to single spaces and trimmed.
/// This removes markup artefacts only; NFKC / punctuation normalization
/// is deliberately left to the downstream `normalize` step.
fn collect_text(el: ElementRef) -> String {
    let raw: String = el.text().collect();
    let mut out = String::with_capacity(raw.len());
    let mut prev_ws = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

fn heading_level(name: &str) -> Option<u8> {
    match name {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

/// Whether an element marks a footnote / endnote body. Checks the EPUB 3
/// `epub:type` vocabulary and the ARIA `role` fallback. The reference
/// marker (`noteref` / `doc-noteref`) is deliberately not matched — it
/// is a pointer, not the note body.
fn is_footnote(el: ElementRef) -> bool {
    for (name, value) in el.value().attrs() {
        let local = name.rsplit(':').next().unwrap_or(name);
        if local == "type"
            && value
                .split_whitespace()
                .any(|t| matches!(t, "footnote" | "endnote" | "rearnote" | "note"))
        {
            return true;
        }
        if name == "role"
            && value
                .split_whitespace()
                .any(|t| matches!(t, "doc-footnote" | "doc-endnote"))
        {
            return true;
        }
    }
    false
}
