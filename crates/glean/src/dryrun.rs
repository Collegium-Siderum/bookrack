// SPDX-License-Identifier: Apache-2.0

//! Paper-side dryrun: pre-vector simulation of what [`glean_paper`]
//! would produce, without writing to the catalog, corpus, vector
//! store, or papers_dir. Peer of [`bookrack_ingest::dryrun`] for the
//! paper pipeline.
//!
//! Each [`dryrun_paper`] call replays EXTRACT (the heavy PDF text-layer
//! read) and runs the same IDENTIFY pass `glean_paper` does
//! (DOI / arXiv / ISSN / venue / year / abstract pick). STRUCTURE is
//! predicted statically from the colored block stream rather than
//! materialised into a corpus, and CHUNK is replayed against the
//! abstract leaf without touching the embedder. The result is a
//! report describing IDENTIFY hit rates, the predicted node tree
//! shape, and the bibliographic fields glean would have written, plus
//! a per-format / per-outcome [`DryrunPaperSummary`] aggregator.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use bookrack_core::NodeId;
use bookrack_extract::{Block, BlockKind, ExtractError, ExtractOutcome};
use serde::{Deserialize, Serialize};

use crate::{AbstractStrategy, ChunkParams, bookrack_audit_profile_default, identify, plan_chunks};

/// Knobs for one dryrun.
#[derive(Debug, Clone, Default)]
pub struct DryrunPaperParams {
    /// Which abstract strategy the IDENTIFY pass uses. Mirrors the
    /// matching field on [`crate::GleanParams`].
    pub abstract_strategy: AbstractStrategy,
    /// CHUNK tuning. The abstract is almost always a single chunk
    /// under the default 1000-character target.
    pub chunk: ChunkParams,
    /// When true, the CHUNK preview is skipped and
    /// [`DryrunPaperReport::predicted_chunks`] stays `None`.
    pub skip_chunks: bool,
}

/// One paper's dryrun outcome.
///
/// Every payload field is optional so a record can describe a
/// successful run, a NeedsOcr route, an unsupported format, or an
/// extract error without changing shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DryrunPaperReport {
    /// The file's stem, identifying it within the report.
    pub stem: String,
    /// Lowercased extension as the format key.
    pub format: String,
    /// File size in bytes.
    pub bytes: u64,
    /// `extracted` / `needs_ocr` / `unsupported` / `error`.
    pub extract_outcome: String,
    /// Adapter the extract layer reported (when extracted).
    pub adapter: Option<String>,
    /// Total `Extraction::blocks.len()`.
    pub blocks: Option<usize>,
    /// Heading-kind block count after the paper structuring pass.
    pub heading_blocks: Option<usize>,
    /// Body-kind block count.
    pub body_blocks: Option<usize>,
    /// Predicted total corpus node count (Work root plus every
    /// organizer and leaf STRUCTURE would emit).
    pub predicted_nodes: Option<usize>,
    /// Predicted Section organizer count under the Work root.
    pub predicted_sections: Option<usize>,
    /// Predicted Subsection organizer count.
    pub predicted_subsections: Option<usize>,
    /// Predicted heading leaf count.
    pub predicted_heading_leaves: Option<usize>,
    /// Predicted body paragraph leaf count.
    pub predicted_body_leaves: Option<usize>,
    /// Predicted figure caption leaf count.
    pub predicted_caption_leaves: Option<usize>,
    /// DOI surfaced by IDENTIFY (envelope biblio first, then the
    /// regex fallback). `None` when no DOI matched.
    pub doi: Option<String>,
    /// arXiv id in canonical form.
    pub arxiv_id: Option<String>,
    /// ISSN in dashed canonical form.
    pub issn: Option<String>,
    /// Container title — journal, conference proceedings, or book
    /// series. Populated from `Biblio::container_title` or the venue
    /// cue scan.
    pub venue: Option<String>,
    /// Source label of the abstract pick:
    /// `"heading" | "first_page_long_para" | "first_long_para"`.
    /// `None` when no body block could serve as the abstract.
    pub abstract_source: Option<String>,
    /// Character count of the picked abstract body, after trimming.
    pub abstract_chars: Option<usize>,
    /// Title after the title-sniff pass.
    pub title: Option<String>,
    /// Publication year IDENTIFY recovered.
    pub year: Option<i32>,
    /// Number of contributors the extractor carried in `Biblio`.
    pub contributor_count: Option<usize>,
    /// Predicted CHUNK count for the abstract under the active
    /// [`ChunkParams`]. `None` when [`DryrunPaperParams::skip_chunks`]
    /// is set; `Some(0)` when there is no abstract to chunk.
    pub predicted_chunks: Option<usize>,
    /// Wall time spent inside the dryrun, in milliseconds.
    pub elapsed_ms: u64,
    /// Carried only when extract returned an error or NeedsOcr.
    pub error: Option<String>,
}

/// The aggregate over a set of [`DryrunPaperReport`]s.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DryrunPaperSummary {
    /// Total files considered.
    pub n_files: usize,
    /// Files for each lowercased extension.
    pub formats: BTreeMap<String, usize>,
    /// `extracted` / `needs_ocr` / `unsupported` / `error` counts.
    pub extract_outcomes: BTreeMap<String, usize>,
    /// Files for which the IDENTIFY pass found a DOI.
    pub doi_hits: usize,
    /// Files for which IDENTIFY found an arXiv id.
    pub arxiv_hits: usize,
    /// Files for which IDENTIFY found a venue / container title.
    pub venue_hits: usize,
    /// Files for which IDENTIFY found an ISSN.
    pub issn_hits: usize,
    /// Files for which the abstract pick succeeded.
    pub abstract_hits: usize,
    /// Abstract pick source histogram.
    pub abstract_sources: BTreeMap<String, usize>,
    /// Files for which IDENTIFY recovered a title.
    pub title_hits: usize,
    /// Files for which IDENTIFY recovered a publication year.
    pub year_hits: usize,
}

/// File extensions the paper dryrun walker considers. Matches the
/// formats `bookrack-extract` understands in paper-shaped layouts.
pub const PAPER_EXTENSIONS: &[&str] = &["pdf", "epub", "html", "htm", "txt"];

/// Dryrun one source file. Never panics: extract failures and
/// pipeline errors are recorded into the returned report rather than
/// propagated.
pub fn dryrun_paper(file: &Path, params: &DryrunPaperParams) -> DryrunPaperReport {
    let started = Instant::now();
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let format = file
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);

    let mut record = DryrunPaperReport {
        stem,
        format,
        bytes,
        extract_outcome: "error".to_string(),
        ..Default::default()
    };

    let audit_profile = bookrack_audit_profile_default();
    let extracted = bookrack_extract::extract(file, &audit_profile, &Default::default());
    let mut extraction = match extracted {
        Ok(ExtractOutcome::Extracted(e)) => e,
        Ok(ExtractOutcome::NeedsOcr { reason }) => {
            record.extract_outcome = "needs_ocr".to_string();
            record.error = Some(reason);
            record.elapsed_ms = started.elapsed().as_millis() as u64;
            return record;
        }
        Err(e) => {
            record.extract_outcome = if matches!(e, ExtractError::UnsupportedFormat { .. }) {
                "unsupported"
            } else {
                "error"
            }
            .to_string();
            record.error = Some(e.to_string());
            record.elapsed_ms = started.elapsed().as_millis() as u64;
            return record;
        }
    };

    // Paper-side structuring pass: color the block stream with heading
    // and caption classifications. PDF only; other adapters pass
    // through with `SourceOfStructure::None`.
    if extraction.provenance.adapter == "pdf" {
        bookrack_extract::pdf_paper::extract_paper_structured(
            &mut extraction.blocks,
            &extraction.toc,
        );
    }

    record.adapter = Some(extraction.provenance.adapter.clone());
    record.blocks = Some(extraction.blocks.len());
    record.heading_blocks = Some(count_blocks(&extraction.blocks, |k| {
        matches!(k, BlockKind::Heading { .. })
    }));
    record.body_blocks = Some(count_blocks(&extraction.blocks, |k| {
        matches!(k, BlockKind::Body)
    }));

    // IDENTIFY pass — same shape `glean_paper` runs.
    let metadata_text = if extraction.provenance.adapter == "pdf" {
        bookrack_extract::extract_paper_metadata_text(file)
            .ok()
            .flatten()
    } else {
        None
    };
    let filename_stem = file.file_stem().map(|s| s.to_string_lossy().into_owned());
    let mut biblio = extraction.biblio.clone();
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
    let abstract_text = abstract_pick.as_ref().map(|(text, _)| text.clone());
    record.abstract_source = abstract_pick.as_ref().map(|(_, src)| (*src).to_string());
    record.abstract_chars = abstract_text
        .as_deref()
        .map(|t| t.trim().chars().count())
        .filter(|n| *n > 0);

    let preview = predict_structure(&extraction.blocks, abstract_text.as_deref());
    record.predicted_nodes = Some(preview.nodes);
    record.predicted_sections = Some(preview.sections);
    record.predicted_subsections = Some(preview.subsections);
    record.predicted_heading_leaves = Some(preview.heading_leaves);
    record.predicted_body_leaves = Some(preview.body_leaves);
    record.predicted_caption_leaves = Some(preview.caption_leaves);

    record.doi = biblio.doi;
    record.arxiv_id = biblio.arxiv_id;
    record.issn = biblio.issn;
    record.venue = biblio.container_title;
    record.title = biblio.title;
    record.year = biblio.year;
    record.contributor_count = Some(biblio.contributors.len());

    if !params.skip_chunks {
        record.predicted_chunks = Some(match abstract_text.as_deref() {
            Some(text) if !text.trim().is_empty() => {
                // `plan_chunks` is a pure function — the leaf id is
                // only stamped onto each PlannedChunk, never dereferenced.
                plan_chunks(NodeId::new(0), text, &params.chunk).len()
            }
            _ => 0,
        });
    }

    record.extract_outcome = "extracted".to_string();
    record.elapsed_ms = started.elapsed().as_millis() as u64;
    record
}

/// Walk a path, dryrun every supported file under it, and return one
/// report per file. `path` may be a single file or a directory; the
/// accepted extension list is [`PAPER_EXTENSIONS`].
pub fn dryrun_path(path: &Path, params: &DryrunPaperParams) -> Vec<DryrunPaperReport> {
    let files = collect_files(path);
    files.iter().map(|p| dryrun_paper(p, params)).collect()
}

/// Aggregate a slice of [`DryrunPaperReport`]s into a single
/// [`DryrunPaperSummary`].
pub fn summarize(reports: &[DryrunPaperReport]) -> DryrunPaperSummary {
    let mut s = DryrunPaperSummary {
        n_files: reports.len(),
        ..Default::default()
    };
    for r in reports {
        *s.formats.entry(r.format.clone()).or_insert(0) += 1;
        *s.extract_outcomes
            .entry(r.extract_outcome.clone())
            .or_insert(0) += 1;
        if r.doi.is_some() {
            s.doi_hits += 1;
        }
        if r.arxiv_id.is_some() {
            s.arxiv_hits += 1;
        }
        if r.venue.is_some() {
            s.venue_hits += 1;
        }
        if r.issn.is_some() {
            s.issn_hits += 1;
        }
        if let Some(source) = r.abstract_source.as_deref() {
            s.abstract_hits += 1;
            *s.abstract_sources.entry(source.to_string()).or_insert(0) += 1;
        }
        if r.title.is_some() {
            s.title_hits += 1;
        }
        if r.year.is_some() {
            s.year_hits += 1;
        }
    }
    s
}

/// Recursively collect every paper-shaped file under `path`. A
/// single-file `path` returns that file when its extension is in
/// [`PAPER_EXTENSIONS`] and an empty list otherwise.
pub fn collect_files(path: &Path) -> Vec<PathBuf> {
    fn matches(p: &Path) -> bool {
        let Some(ext) = p.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        let lower = ext.to_ascii_lowercase();
        PAPER_EXTENSIONS
            .iter()
            .any(|e| e.eq_ignore_ascii_case(&lower))
    }
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                visit(&p, out);
            } else if matches(&p) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    if path.is_dir() {
        visit(path, &mut out);
        out.sort();
    } else if matches(path) {
        out.push(path.to_path_buf());
    }
    out
}

struct StructurePreview {
    nodes: usize,
    sections: usize,
    subsections: usize,
    heading_leaves: usize,
    body_leaves: usize,
    caption_leaves: usize,
}

/// Mirror the counting half of [`crate::build_structure`]: produce the
/// node-tree shape STRUCTURE would emit without allocating ids or
/// inserting any rows. Same orphan-`Heading{2}` promotion rule.
fn predict_structure(blocks: &[Block], abstract_text: Option<&str>) -> StructurePreview {
    let mut preview = StructurePreview {
        // The Work root is always written.
        nodes: 1,
        sections: 0,
        subsections: 0,
        heading_leaves: 0,
        body_leaves: 0,
        caption_leaves: 0,
    };
    if abstract_text.map(|t| !t.trim().is_empty()).unwrap_or(false) {
        preview.nodes += 1;
    }
    let mut current_section = false;
    for block in blocks {
        if block.text.trim().is_empty() {
            continue;
        }
        match block.kind {
            BlockKind::Abstract => continue,
            BlockKind::Heading { level } if level <= 1 => {
                preview.sections += 1;
                preview.heading_leaves += 1;
                // section organizer + heading leaf
                preview.nodes += 2;
                current_section = true;
            }
            BlockKind::Heading { level: 2 } => {
                if !current_section {
                    preview.sections += 1;
                    // implicit section organizer auto-opened by
                    // `build_structure` to host the orphan subsection
                    preview.nodes += 1;
                    current_section = true;
                }
                preview.subsections += 1;
                preview.heading_leaves += 1;
                // subsection organizer + heading leaf
                preview.nodes += 2;
            }
            BlockKind::Heading { .. } => {
                preview.heading_leaves += 1;
                preview.nodes += 1;
            }
            BlockKind::Body => {
                preview.body_leaves += 1;
                preview.nodes += 1;
            }
            BlockKind::Caption => {
                preview.caption_leaves += 1;
                preview.nodes += 1;
            }
            BlockKind::Footnote | BlockKind::Other => continue,
        }
    }
    preview
}

fn count_blocks(blocks: &[Block], mut pred: impl FnMut(&BlockKind) -> bool) -> usize {
    blocks.iter().filter(|b| pred(&b.kind)).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_extract::{Block, BlockKind};

    fn body(text: &str) -> Block {
        Block {
            kind: BlockKind::Body,
            text: text.to_string(),
            source_unit: 1,
            style: None,
        }
    }

    fn heading(level: u8, text: &str) -> Block {
        Block {
            kind: BlockKind::Heading { level },
            text: text.to_string(),
            source_unit: 1,
            style: None,
        }
    }

    #[test]
    fn predict_structure_counts_section_and_body_leaves() {
        let blocks = vec![
            heading(1, "Introduction"),
            body("Lorem ipsum"),
            heading(1, "Methods"),
            body("Tools we used"),
            body("More on tools"),
        ];
        let p = predict_structure(&blocks, Some("This is the abstract."));
        // Work + abstract + (section + heading_leaf) * 2 + body * 3
        assert_eq!(p.nodes, 1 + 1 + 2 * 2 + 3);
        assert_eq!(p.sections, 2);
        assert_eq!(p.subsections, 0);
        assert_eq!(p.heading_leaves, 2);
        assert_eq!(p.body_leaves, 3);
    }

    #[test]
    fn predict_structure_auto_opens_a_section_for_an_orphan_subsection() {
        let blocks = vec![heading(2, "Background"), body("Lorem")];
        let p = predict_structure(&blocks, None);
        // Work + implicit section + subsection organizer + heading leaf + body leaf
        assert_eq!(p.nodes, 1 + 1 + 1 + 1 + 1);
        assert_eq!(p.sections, 1);
        assert_eq!(p.subsections, 1);
        assert_eq!(p.heading_leaves, 1);
        assert_eq!(p.body_leaves, 1);
    }

    #[test]
    fn predict_structure_skips_empty_and_abstract_blocks() {
        let blocks = vec![
            Block {
                kind: BlockKind::Abstract,
                text: "Already in the abstract".to_string(),
                source_unit: 1,
                style: None,
            },
            body("   "),
            body("Lorem"),
        ];
        let p = predict_structure(&blocks, Some("the abstract"));
        // Work + abstract + one body
        assert_eq!(p.nodes, 1 + 1 + 1);
        assert_eq!(p.body_leaves, 1);
    }

    #[test]
    fn summarize_aggregates_hit_rates_per_field() {
        let reports = vec![
            DryrunPaperReport {
                format: "pdf".to_string(),
                extract_outcome: "extracted".to_string(),
                doi: Some("10.1/abc".to_string()),
                arxiv_id: Some("2304.00001".to_string()),
                venue: Some("NeurIPS".to_string()),
                abstract_source: Some("heading".to_string()),
                title: Some("Attention Is All You Need".to_string()),
                year: Some(2017),
                ..Default::default()
            },
            DryrunPaperReport {
                format: "pdf".to_string(),
                extract_outcome: "extracted".to_string(),
                doi: None,
                abstract_source: Some("first_page_long_para".to_string()),
                title: Some("Some Other Paper".to_string()),
                ..Default::default()
            },
            DryrunPaperReport {
                format: "pdf".to_string(),
                extract_outcome: "needs_ocr".to_string(),
                error: Some("no text layer".to_string()),
                ..Default::default()
            },
        ];
        let s = summarize(&reports);
        assert_eq!(s.n_files, 3);
        assert_eq!(s.formats.get("pdf"), Some(&3));
        assert_eq!(s.extract_outcomes.get("extracted"), Some(&2));
        assert_eq!(s.extract_outcomes.get("needs_ocr"), Some(&1));
        assert_eq!(s.doi_hits, 1);
        assert_eq!(s.arxiv_hits, 1);
        assert_eq!(s.venue_hits, 1);
        assert_eq!(s.abstract_hits, 2);
        assert_eq!(s.abstract_sources.get("heading"), Some(&1));
        assert_eq!(s.abstract_sources.get("first_page_long_para"), Some(&1));
        assert_eq!(s.title_hits, 2);
        assert_eq!(s.year_hits, 1);
    }
}
