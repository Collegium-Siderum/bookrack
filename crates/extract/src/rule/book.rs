// SPDX-License-Identifier: Apache-2.0

//! Book-side rule extraction (steps 6–11): unit titles, body headings,
//! paragraph and line block splitting, TOC ordering and deduplication,
//! completeness check, bibliographic metadata, and [`Provenance`].

use std::path::Path;

use bookrack_audit_profile::HeadingPatterns;
use regex::Regex;

use super::source::{CompletenessReport, RuleSource, SourceUnit};
use super::spec::{
    BiblioSpec, BlockSplit, HeadingSource, MatchOn, RuleFile, TextLayer, UnitTitles,
};
use super::{ADAPTER, RULE_ENGINE_VERSION, RuleError};
use crate::contract::{
    Biblio, Block, BlockKind, Contributor, ContributorRole, Extraction, Provenance, RuleOrigin,
    TextLayerQuality, Toc, TocEntry,
};

impl RuleFile {
    /// Run the book-side rule steps over `source` and return the
    /// [`Extraction`].
    pub fn extract(
        &self,
        source: &RuleSource,
        heading_patterns: &HeadingPatterns,
    ) -> Result<Extraction, RuleError> {
        Ok(self.extract_with_report(source, heading_patterns)?.0)
    }

    /// Run the book-side rule steps over `source` and return both the
    /// [`Extraction`] and the [`CompletenessReport`].
    pub fn extract_with_report(
        &self,
        source: &RuleSource,
        heading_patterns: &HeadingPatterns,
    ) -> Result<(Extraction, CompletenessReport), RuleError> {
        let units = self.units(source)?;
        let mut blocks: Vec<Block> = Vec::new();
        let mut toc_raw: Vec<(usize, u8, String, bool)> = Vec::new();

        for unit in &units {
            let first_block = blocks.len();
            let mut iter = unit.lines.iter().peekable();

            if self.rule.structure.unit_titles == UnitTitles::FirstLine {
                while let Some(line) = iter.peek() {
                    if line.trim().is_empty() {
                        iter.next();
                    } else {
                        break;
                    }
                }
                if let Some(line) = iter.next() {
                    let (depth, title) = self.match_unit_title(line);
                    let block_idx = blocks.len();
                    blocks.push(Block {
                        kind: BlockKind::Heading {
                            level: depth.saturating_add(1),
                        },
                        text: title.clone(),
                        source_unit: unit.index,
                        style: None,
                    });
                    toc_raw.push((block_idx, depth, title, true));
                }
            }

            let mut para: Vec<&str> = Vec::new();
            for line in iter {
                if line.trim().is_empty() {
                    if self.rule.blocks.split == BlockSplit::BlankLine {
                        flush_paragraph(&mut para, unit.index, &mut blocks);
                    }
                    continue;
                }

                if let Some((depth, level, label)) = self.match_body_heading(line, heading_patterns)
                {
                    flush_paragraph(&mut para, unit.index, &mut blocks);
                    let block_idx = blocks.len();
                    blocks.push(Block {
                        kind: BlockKind::Heading { level },
                        text: line.trim().to_string(),
                        source_unit: unit.index,
                        style: None,
                    });
                    toc_raw.push((block_idx, depth, label, false));
                    continue;
                }

                match self.rule.blocks.split {
                    BlockSplit::BlankLine => para.push(line.trim()),
                    BlockSplit::Line => blocks.push(Block {
                        kind: BlockKind::Body,
                        text: line.trim().to_string(),
                        source_unit: unit.index,
                        style: None,
                    }),
                }
            }
            flush_paragraph(&mut para, unit.index, &mut blocks);

            if self.rule.structure.unit_titles == UnitTitles::Filename && blocks.len() > first_block
            {
                let raw_label = filename_stem_label(&unit.member, &self.number);
                let (depth, label) = self.match_unit_title(&raw_label);
                toc_raw.push((first_block, depth, label, true));
            }
        }

        toc_raw.sort_by_key(|(start_block, _, _, is_unit_title)| (*start_block, !*is_unit_title));
        toc_raw.dedup_by_key(|(start_block, _, _, _)| *start_block);
        let toc = Toc {
            entries: toc_raw
                .into_iter()
                .map(|(start_block, depth, label, _)| TocEntry {
                    label,
                    depth,
                    start_block: Some(start_block),
                })
                .collect(),
        };

        if !blocks.iter().any(|b| matches!(b.kind, BlockKind::Body)) {
            return Err(RuleError::EmptyExtraction {
                rule: self.path.clone(),
            });
        }

        let mut fallbacks = source.fallbacks.clone();
        let report = self.check_completeness(&units, &mut fallbacks)?;
        let biblio = build_biblio(&self.rule.biblio);
        let text_layer_quality = match self.rule.provenance.text_layer {
            TextLayer::BornDigital => TextLayerQuality::BornDigital,
            TextLayer::Doubtful => TextLayerQuality::Doubtful,
        };
        let provenance = Provenance {
            adapter: ADAPTER.to_string(),
            extractor_version: crate::EXTRACTOR_VERSION,
            text_layer_quality,
            skipped_units: Vec::new(),
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
            fallbacks,
            rule: Some(RuleOrigin {
                fingerprint: self.fingerprint(source),
                engine_version: RULE_ENGINE_VERSION,
                units: units.iter().map(SourceUnit::origin).collect(),
            }),
        };

        Ok((
            Extraction {
                blocks,
                toc,
                biblio,
                provenance,
            },
            report,
        ))
    }

    fn match_target<'a>(&self, line: &'a str) -> &'a str {
        match self.rule.structure.match_on {
            MatchOn::Trimmed => line.trim(),
            MatchOn::Raw => line,
        }
    }

    fn match_unit_title(&self, line: &str) -> (u8, String) {
        let target = self.match_target(line);
        for (re, depth) in &self.unit_levels {
            if let Some(caps) = re.captures(target) {
                let label = caps
                    .name("label")
                    .map(|m| m.as_str().trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| line.trim().to_string());
                return (*depth, label);
            }
        }
        (0, line.trim().to_string())
    }

    fn match_body_heading(
        &self,
        line: &str,
        heading_patterns: &HeadingPatterns,
    ) -> Option<(u8, u8, String)> {
        let target = self.match_target(line);
        match self.rule.structure.headings {
            HeadingSource::None => None,
            HeadingSource::Builtin => crate::headings::heading_level(target, heading_patterns)
                .map(|level| (level.saturating_sub(1), level, line.trim().to_string())),
            HeadingSource::Patterns => {
                for (re, depth) in &self.patterns {
                    if let Some(caps) = re.captures(target) {
                        let label = caps
                            .name("label")
                            .map(|m| m.as_str().trim().to_string())
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| line.trim().to_string());
                        return Some((*depth, depth.saturating_add(1), label));
                    }
                }
                None
            }
        }
    }
}

fn flush_paragraph(para: &mut Vec<&str>, source_unit: u32, blocks: &mut Vec<Block>) {
    if para.is_empty() {
        return;
    }
    let text = para.join(" ");
    para.clear();
    if !text.is_empty() {
        blocks.push(Block {
            kind: BlockKind::Body,
            text,
            source_unit,
            style: None,
        });
    }
}

fn filename_stem_label(member: &str, number_re: &Regex) -> String {
    let stem = Path::new(member)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(member);
    let stripped = match number_re.find(stem) {
        Some(m) if m.start() == 0 => &stem[m.end()..],
        _ => stem,
    };
    let cleaned = stripped
        .trim_start_matches(|c: char| c == '_' || c == '-' || c == '.' || c.is_whitespace());
    if cleaned.is_empty() {
        stem.to_string()
    } else {
        cleaned.to_string()
    }
}

fn build_biblio(spec: &BiblioSpec) -> Biblio {
    Biblio {
        title: spec.title.clone(),
        subtitle: spec.subtitle.clone(),
        publisher: spec.publisher.clone(),
        year: spec.year,
        year_raw: spec.year.map(|y| y.to_string()),
        isbn: spec.isbn.clone(),
        series: spec.series.clone(),
        language: spec.language.clone(),
        contributors: spec
            .authors
            .iter()
            .map(|name| Contributor {
                name: name.clone(),
                role: ContributorRole::Author,
                family: None,
                given: None,
                orcid: None,
            })
            .collect(),
        ..Biblio::default()
    }
}
