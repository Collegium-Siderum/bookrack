// SPDX-License-Identifier: Apache-2.0

//! From a checked rule to its source: the members it names, their
//! canonical text, and the two values that identify the result.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::spec::{Expect, Frontmatter, MemberOrder, RuleFile, UnitsBy};
use super::{ADAPTER, RULE_ENGINE_VERSION, RuleError, glob};
use crate::contract::{FallbackEvent, UnitOrigin, fallback_kinds};

/// Joins two members in the canonical text. A constant of the source
/// hash: no rule can set it.
const MEMBER_SEPARATOR: &str = "\n\n";

/// First line of the bytes a fingerprint is the hash of.
const FINGERPRINT_HEADER: &str = "rule-fingerprint/1";

/// A member file a rule names, before it is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFile {
    /// The member's `/`-separated path relative to the rule file's
    /// directory.
    pub relative: String,
    /// The path to read the member by.
    pub path: PathBuf,
}

/// One member's place in a [`RuleSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The member's `/`-separated path relative to the rule file's
    /// directory.
    pub relative: String,
    /// The bytes of [`RuleSource::text`] that are this member's
    /// canonical text. Empty for a member with no text.
    pub span: Range<usize>,
}

/// The members of a rule, read into one canonical text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSource {
    /// The canonical text: every member decoded, its line endings made
    /// `\n`, trailing whitespace removed from each line and trailing
    /// line breaks from the member, and the members joined by one
    /// blank line.
    pub text: String,
    /// The members in reading order.
    pub members: Vec<Member>,
    /// Decoding fallbacks taken while reading, each naming its member.
    pub fallbacks: Vec<FallbackEvent>,
}

/// One unit cut from a [`RuleSource`] according to `[units]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUnit {
    /// 0-based unit index (`Block::source_unit`).
    pub index: u32,
    /// Relative path of the first member file that contributed to this
    /// unit.
    pub member: String,
    /// Printed page label captured from a marker's `label` group, if
    /// any.
    pub label: Option<String>,
    /// 1-based sheet or unit number captured from a marker's `sheet`
    /// group or a member filename's `completeness.number` `n` group.
    pub sheet: Option<u32>,
    /// The unit's canonical lines joined by `\n` and trimmed at both
    /// ends.
    pub text: String,
    /// The unit's canonical lines in order, with leading whitespace and
    /// internal blank lines intact for book-side heading and paragraph
    /// scanning.
    pub(super) lines: Vec<String>,
}

impl SourceUnit {
    /// The [`UnitOrigin`] recorded for this unit in
    /// [`crate::RuleOrigin`].
    pub fn origin(&self) -> UnitOrigin {
        UnitOrigin {
            member: self.member.clone(),
            label: self.label.clone(),
            sheet: self.sheet,
        }
    }
}

/// Result of checking unit numbers against `[completeness]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletenessReport {
    /// Count of units that carried a number.
    pub numbered: usize,
    /// Lowest unit number seen, if any.
    pub min: Option<u32>,
    /// Highest unit number seen, if any.
    pub max: Option<u32>,
    /// Missing numbers between `min` and `max` under
    /// `expect = "contiguous"`.
    pub gaps: Vec<u32>,
    /// Numbers that appeared more than once under
    /// `expect = "contiguous"`.
    pub duplicates: Vec<u32>,
    /// Whether the observed unit numbers were monotonically
    /// non-decreasing.
    pub ordered: bool,
}

impl RuleSource {
    /// SHA-256 of the canonical text's UTF-8 bytes, as lowercase hex.
    ///
    /// The members' encodings, byte-order marks, line-ending
    /// conventions, trailing whitespace, and trailing line breaks do
    /// not reach it; leading whitespace does.
    pub fn source_sha256(&self) -> String {
        format!("{:x}", Sha256::digest(self.text.as_bytes()))
    }

    /// Length of the canonical text in bytes.
    pub fn byte_size(&self) -> u64 {
        self.text.len() as u64
    }
}

impl RuleFile {
    /// The member files the rule names, in reading order.
    ///
    /// The rule file's directory is searched, symbolic links followed;
    /// a file is a member when its relative path matches a pattern of
    /// `members.include` and none of `members.exclude`. Under
    /// `members.order = "explicit"` the members are the listed paths
    /// instead, and a matching file the list leaves out is an error.
    pub fn members(&self) -> Result<Vec<MemberFile>, RuleError> {
        let found = glob::files_under(&self.dir)
            .map_err(|(path, source)| RuleError::MemberRead { path, source })?;
        let mut matched: Vec<MemberFile> = found
            .into_iter()
            .filter(|(relative, _)| {
                self.include.iter().any(|p| p.is_match(relative))
                    && !self.exclude.iter().any(|p| p.is_match(relative))
            })
            .map(|(relative, path)| MemberFile { relative, path })
            .collect();
        let spec = &self.rule.members;
        match spec.order {
            // `files_under` returns the files sorted bytewise by relative path.
            MemberOrder::Name => {}
            MemberOrder::Natural => {
                matched.sort_by(|a, b| glob::natural_cmp(&a.relative, &b.relative));
            }
            MemberOrder::Explicit => {
                let listed: BTreeSet<&str> = spec.explicit.iter().map(String::as_str).collect();
                if let Some(extra) = matched
                    .iter()
                    .find(|m| !listed.contains(m.relative.as_str()))
                {
                    return Err(RuleError::MemberUnlisted {
                        rule: self.path.clone(),
                        member: extra.relative.clone(),
                    });
                }
                matched = Vec::with_capacity(spec.explicit.len());
                for relative in &spec.explicit {
                    let path = self.dir.join(relative);
                    if !path.is_file() {
                        return Err(RuleError::MemberMissing {
                            rule: self.path.clone(),
                            member: relative.clone(),
                        });
                    }
                    matched.push(MemberFile {
                        relative: relative.clone(),
                        path,
                    });
                }
            }
        }
        if matched.is_empty() {
            return Err(RuleError::NoMembers {
                rule: self.path.clone(),
            });
        }
        Ok(matched)
    }

    /// Read the rule's members into their canonical text.
    pub fn read(&self) -> Result<RuleSource, RuleError> {
        let mut text = String::new();
        let mut members = Vec::new();
        let mut fallbacks = Vec::new();
        for (i, file) in self.members()?.into_iter().enumerate() {
            let bytes = std::fs::read(&file.path).map_err(|source| RuleError::MemberRead {
                path: file.path.clone(),
                source,
            })?;
            let decoded = crate::txt::decode(&bytes, ADAPTER, Some(&file.relative), &mut fallbacks);
            if i > 0 {
                text.push_str(MEMBER_SEPARATOR);
            }
            let start = text.len();
            push_canonical(&mut text, &decoded);
            members.push(Member {
                relative: file.relative,
                span: start..text.len(),
            });
        }
        Ok(RuleSource {
            text,
            members,
            fallbacks,
        })
    }

    /// The fingerprint of this rule over `source`: SHA-256, as
    /// lowercase hex, of the engine version, the parsed rule, and the
    /// member layout.
    ///
    /// The parsed rule enters as its serialized form, so a rule file
    /// edited only in its whitespace or comments keeps its fingerprint.
    /// The layout is each member's relative path and the length of its
    /// canonical text: two files merged into one change it even when
    /// the canonical text stays byte-identical, and moving the rule
    /// and its members together does not.
    pub fn fingerprint(&self, source: &RuleSource) -> String {
        format!("{:x}", Sha256::digest(self.fingerprint_input(source)))
    }

    /// The bytes [`RuleFile::fingerprint`] hashes: a header line, the
    /// engine version, the rule as one line of JSON, then one line per
    /// member holding its relative path, a tab, and the byte length of
    /// its canonical text.
    fn fingerprint_input(&self, source: &RuleSource) -> Vec<u8> {
        let rule = serde_json::to_string(&self.rule).expect("a rule serializes to JSON");
        let mut input = format!("{FINGERPRINT_HEADER}\n{RULE_ENGINE_VERSION}\n{rule}\n");
        for member in &source.members {
            input.push_str(&format!("{}\t{}\n", member.relative, member.span.len()));
        }
        input.into_bytes()
    }

    /// Cut `source` into [`SourceUnit`]s after stripping any
    /// rule-configured YAML frontmatter.
    pub fn units(&self, source: &RuleSource) -> Result<Vec<SourceUnit>, RuleError> {
        match self.rule.units.by {
            UnitsBy::File => {
                let mut out = Vec::with_capacity(source.members.len());
                for (idx, member) in source.members.iter().enumerate() {
                    let lines: Vec<String> = self
                        .member_lines(source, idx)
                        .into_iter()
                        .map(|(line, _)| line)
                        .collect();
                    let file_name = Path::new(&member.relative)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&member.relative);
                    let sheet = self.number_u32(file_name);
                    let text = lines.join("\n").trim().to_string();
                    out.push(SourceUnit {
                        index: idx as u32,
                        member: member.relative.clone(),
                        label: None,
                        sheet,
                        text,
                        lines,
                    });
                }
                Ok(out)
            }
            UnitsBy::Whole => {
                let mut lines = Vec::new();
                for idx in 0..source.members.len() {
                    if idx > 0 {
                        lines.push(String::new());
                    }
                    lines.extend(
                        self.member_lines(source, idx)
                            .into_iter()
                            .map(|(line, _)| line),
                    );
                }
                let text = lines.join("\n").trim().to_string();
                Ok(vec![SourceUnit {
                    index: 0,
                    member: source.members[0].relative.clone(),
                    label: None,
                    sheet: None,
                    text,
                    lines,
                }])
            }
            UnitsBy::Marker => {
                let marker_re = self.marker.as_ref().expect("marker regex checked at load");
                let mut out: Vec<SourceUnit> = Vec::new();
                for idx in 0..source.members.len() {
                    if idx > 0
                        && let Some(last) = out.last_mut()
                    {
                        last.lines.push(String::new());
                    }
                    let member_rel = &source.members[idx].relative;
                    for (line, line_no) in self.member_lines(source, idx) {
                        let probe = line.trim();
                        if let Some(caps) = marker_re.captures(probe) {
                            let label = caps
                                .name("label")
                                .map(|m| m.as_str().trim().to_string())
                                .filter(|s| !s.is_empty());
                            let sheet = caps.name("sheet").and_then(|m| {
                                let s = m.as_str().trim();
                                self.number_u32(s).or_else(|| s.parse::<u32>().ok())
                            });
                            let index = out.len() as u32;
                            out.push(SourceUnit {
                                index,
                                member: member_rel.clone(),
                                label,
                                sheet,
                                text: String::new(),
                                lines: Vec::new(),
                            });
                        } else if let Some(last) = out.last_mut() {
                            last.lines.push(line);
                        } else if !probe.is_empty() {
                            return Err(RuleError::Units {
                                rule: self.path.clone(),
                                detail: format!(
                                    "content before the first marker in {member_rel} at line {line_no}"
                                ),
                            });
                        }
                    }
                }
                if out.is_empty() {
                    return Err(RuleError::Units {
                        rule: self.path.clone(),
                        detail: String::from("no unit marker matched any line"),
                    });
                }
                for unit in &mut out {
                    unit.text = unit.lines.join("\n").trim().to_string();
                }
                Ok(out)
            }
            UnitsBy::Adapter => unreachable!("refused at load"),
        }
    }

    /// Check `units` against `[completeness]`, recording any
    /// non-contiguous fallbacks on `fallbacks` and failing with
    /// [`RuleError::Completeness`] when `fail_on_gap` is set.
    pub fn check_completeness(
        &self,
        units: &[SourceUnit],
        fallbacks: &mut Vec<FallbackEvent>,
    ) -> Result<CompletenessReport, RuleError> {
        if self.rule.units.by == UnitsBy::Whole {
            return Ok(CompletenessReport {
                numbered: 0,
                min: None,
                max: None,
                gaps: Vec::new(),
                duplicates: Vec::new(),
                ordered: true,
            });
        }
        let numbers: Vec<u32> = units.iter().filter_map(|u| u.sheet).collect();
        let ordered = numbers.windows(2).all(|w| w[0] <= w[1]);
        if numbers.is_empty() || self.rule.completeness.expect == Expect::None {
            return Ok(CompletenessReport {
                numbered: numbers.len(),
                min: numbers.iter().copied().min(),
                max: numbers.iter().copied().max(),
                gaps: Vec::new(),
                duplicates: Vec::new(),
                ordered,
            });
        }

        let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
        for &n in &numbers {
            *counts.entry(n).or_insert(0) += 1;
        }
        let min = *counts.keys().next().expect("numbers is non-empty");
        let max = *counts.keys().next_back().expect("numbers is non-empty");
        let duplicates: Vec<u32> = counts
            .iter()
            .filter(|(_, c)| **c > 1)
            .map(|(n, _)| *n)
            .collect();
        let gaps: Vec<u32> = (min..=max)
            .filter(|n| !counts.contains_key(n))
            .take(1024)
            .collect();

        if !gaps.is_empty() {
            FallbackEvent::record(
                fallbacks,
                ADAPTER,
                fallback_kinds::RULE_GAP,
                Some(join_numbers(&gaps)),
            );
        }
        if !duplicates.is_empty() {
            FallbackEvent::record(
                fallbacks,
                ADAPTER,
                fallback_kinds::RULE_DUPLICATE,
                Some(join_numbers(&duplicates)),
            );
        }
        if !ordered {
            FallbackEvent::record(
                fallbacks,
                ADAPTER,
                fallback_kinds::RULE_ORDER,
                Some(join_numbers(&numbers)),
            );
        }

        if self.rule.completeness.fail_on_gap
            && (!gaps.is_empty() || !duplicates.is_empty() || !ordered)
        {
            return Err(RuleError::Completeness {
                rule: self.path.clone(),
                gaps,
                duplicates,
                ordered,
            });
        }

        Ok(CompletenessReport {
            numbered: numbers.len(),
            min: Some(min),
            max: Some(max),
            gaps,
            duplicates,
            ordered,
        })
    }

    /// Lines of `source.members[member_idx]` after stripping any
    /// rule-configured YAML frontmatter, paired with their 1-based line
    /// numbers inside that member.
    fn member_lines(&self, source: &RuleSource, member_idx: usize) -> Vec<(String, usize)> {
        let member = &source.members[member_idx];
        let raw = &source.text[member.span.clone()];
        let strip_fm = match self.rule.members.frontmatter {
            Frontmatter::None => false,
            Frontmatter::First => member_idx == 0,
            Frontmatter::Each => true,
        };
        let body = if strip_fm {
            crate::ocr::strip_frontmatter(raw)
        } else {
            raw
        };
        if body.is_empty() {
            return Vec::new();
        }
        let prefix_len = raw.len() - body.len();
        let skipped_lines = raw.as_bytes()[..prefix_len]
            .iter()
            .filter(|&&b| b == b'\n')
            .count();
        body.split('\n')
            .enumerate()
            .map(|(i, line)| (line.to_string(), skipped_lines + i + 1))
            .collect()
    }
}

fn join_numbers(nums: &[u32]) -> String {
    nums.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Append one member's canonical text to `out`: `\r\n` and a lone `\r`
/// read as line breaks, trailing whitespace dropped from every line,
/// and the line breaks that end the member dropped.
fn push_canonical(out: &mut String, decoded: &str) {
    let start = out.len();
    let unified = decoded.replace("\r\n", "\n").replace('\r', "\n");
    for (i, line) in unified.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    let kept = start + out[start..].trim_end_matches('\n').len();
    out.truncate(kept);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(decoded: &str) -> String {
        let mut out = String::new();
        push_canonical(&mut out, decoded);
        out
    }

    #[test]
    fn every_line_ending_becomes_a_line_feed() {
        assert_eq!(canonical("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }

    #[test]
    fn trailing_whitespace_goes_and_leading_whitespace_stays() {
        assert_eq!(canonical("  a \t\n\u{3000}b\u{3000}\n"), "  a\n\u{3000}b");
    }

    #[test]
    fn the_line_breaks_that_end_a_member_go_and_inner_blank_lines_stay() {
        assert_eq!(canonical("a\n\n\nb\n \n\r\n\n"), "a\n\n\nb");
        assert_eq!(canonical("\n\na"), "\n\na");
    }

    #[test]
    fn a_member_of_whitespace_alone_is_empty() {
        assert_eq!(canonical(" \r\n\t\n"), "");
        assert_eq!(canonical(""), "");
    }

    #[test]
    fn a_byte_order_mark_past_the_start_is_kept() {
        assert_eq!(canonical("a\n\u{feff}b"), "a\n\u{feff}b");
    }

    /// The fingerprint input of a rule that sets only its required
    /// keys, over one member of three bytes and one empty member. The
    /// second line is `RULE_ENGINE_VERSION`: the serialized rule and
    /// the layout lines are fixed under it.
    const MINIMAL_INPUT: &str = concat!(
        "rule-fingerprint/1\n",
        "1\n",
        r#"{"schema":1,"kind":"book","#,
        r#""members":{"include":["**/*.txt"],"exclude":[],"order":"natural","explicit":[],"frontmatter":"none"},"#,
        r#""units":{"by":"file","marker":null},"#,
        r#""blocks":{"split":"blank-line"},"#,
        r#""structure":{"unit_titles":"first-line","unit_levels":[],"headings":"builtin","patterns":[],"match":"trimmed"},"#,
        r#""biblio":{"title":null,"subtitle":null,"authors":[],"publisher":null,"year":null,"isbn":null,"series":null,"language":null},"#,
        r#""provenance":{"text_layer":"born-digital"},"#,
        r#""completeness":{"number":"^(?P<n>\\d+)","expect":"contiguous","fail_on_gap":false}}"#,
        "\n",
        "a.txt\t3\n",
        "sub/b.txt\t0\n",
    );

    #[test]
    fn the_fingerprint_input_is_the_version_the_rule_and_the_member_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rule = dir.path().join("rule.toml");
        std::fs::write(
            &rule,
            "schema = 1\n[members]\ninclude = [\"**/*.txt\"]\n[units]\nby = \"file\"\n",
        )
        .expect("write rule");
        std::fs::write(dir.path().join("a.txt"), "one\n").expect("write a");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub/b.txt"), " \n").expect("write b");

        let file = RuleFile::load(&rule).expect("load");
        let source = file.read().expect("read");
        let input = String::from_utf8(file.fingerprint_input(&source)).expect("utf-8");
        assert_eq!(
            input, MINIMAL_INPUT,
            "the fingerprint input moved: bump RULE_ENGINE_VERSION and restate it here",
        );
        assert_eq!(
            file.fingerprint(&source),
            format!("{:x}", Sha256::digest(input.as_bytes())),
        );
    }

    #[test]
    fn appending_leaves_what_is_already_there_alone() {
        let mut out = String::from("x\n\n");
        push_canonical(&mut out, "\n\n");
        assert_eq!(out, "x\n\n");
    }
}
