// SPDX-License-Identifier: Apache-2.0

//! From a checked rule to its source: the members it names, their
//! canonical text, and the two values that identify the result.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use super::spec::{MemberOrder, RuleFile};
use super::{ADAPTER, RULE_ENGINE_VERSION, RuleError, glob};
use crate::contract::FallbackEvent;

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
