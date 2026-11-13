// SPDX-License-Identifier: Apache-2.0

//! Why a rule could not be read into a source.

use std::path::PathBuf;

use bookrack_core::{Explain, Problem};

/// Why a rule file, or the members it names, could not be read.
///
/// `Display` is a self-sufficient single line for the log; the
/// three-part wording is assembled in the [`Explain`] impl.
#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    /// The rule file is unreadable, is not TOML of the rule schema, or
    /// sets a key to a value the engine refuses.
    #[error("cannot read rule file \"{}\": {detail}", path.display())]
    Parse {
        /// The rule file.
        path: PathBuf,
        /// What is wrong, on one line: the key at fault and its line
        /// when the fault lies in one.
        detail: String,
    },

    /// The member patterns match no file.
    #[error("rule \"{}\" matches no member files", rule.display())]
    NoMembers {
        /// The rule file.
        rule: PathBuf,
    },

    /// `members.explicit` names a path that is not a file.
    #[error("rule \"{}\" lists a member that does not exist: \"{member}\"", rule.display())]
    MemberMissing {
        /// The rule file.
        rule: PathBuf,
        /// The listed path, relative to the rule file's directory.
        member: String,
    },

    /// Under `members.order = "explicit"`, the patterns match a file
    /// the list does not name.
    #[error(
        "rule \"{}\" matches a member its explicit list leaves out: \"{member}\"",
        rule.display()
    )]
    MemberUnlisted {
        /// The rule file.
        rule: PathBuf,
        /// The matched path, relative to the rule file's directory.
        member: String,
    },

    /// A member file, or a directory searched for members, could not be
    /// read.
    #[error("could not read member \"{}\": {source}", path.display())]
    MemberRead {
        /// The file or directory that could not be read.
        path: PathBuf,
        /// The failing read.
        #[source]
        source: std::io::Error,
    },

    /// The canonical text could not be cut into units according to
    /// `[units]`.
    #[error("cannot split \"{}\" into units: {detail}", rule.display())]
    Units {
        /// The rule file.
        rule: PathBuf,
        /// Which member and line failed, or why no unit could be cut.
        detail: String,
    },

    /// `completeness.expect = "contiguous"` and `completeness.fail_on_gap = true`
    /// saw missing, repeated, or out-of-order unit numbers.
    #[error(
        "units of \"{}\" are not contiguous: {}",
        rule.display(),
        completeness_detail(gaps, duplicates, *ordered)
    )]
    Completeness {
        /// The rule file.
        rule: PathBuf,
        /// Missing unit numbers between the lowest and highest numbered
        /// units.
        gaps: Vec<u32>,
        /// Unit numbers that appeared more than once.
        duplicates: Vec<u32>,
        /// Whether the observed unit numbers were monotonically
        /// non-decreasing.
        ordered: bool,
    },

    /// Extraction produced zero body blocks.
    #[error("extraction of \"{}\" produced no body blocks", rule.display())]
    EmptyExtraction {
        /// The rule file.
        rule: PathBuf,
    },
}

fn completeness_detail(gaps: &[u32], duplicates: &[u32], ordered: bool) -> String {
    let mut parts = Vec::new();
    if !gaps.is_empty() {
        let list = gaps
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("missing: {list}"));
    }
    if !duplicates.is_empty() {
        let list = duplicates
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("duplicate: {list}"));
    }
    if !ordered {
        parts.push(String::from("out of order"));
    }
    parts.join("; ")
}

impl Explain for RuleError {
    fn explain(&self) -> Problem {
        match self {
            RuleError::Parse { path, detail } => {
                Problem::new(format!("cannot read rule file \"{}\"", path.display()))
                    .detail(format!("The file was refused: {detail}."))
                    .hint(
                        "Correct the key the detail names. docs/operating.md describes the \
                         rule format.",
                    )
            }

            RuleError::NoMembers { rule } => Problem::new(format!(
                "rule \"{}\" matches no member files",
                rule.display()
            ))
            .hint(
                "A member pattern is matched against paths relative to the rule file's \
                 directory. `*` stays inside one directory; write `**/` to descend.",
            ),

            RuleError::MemberMissing { rule, member } => Problem::new(format!(
                "rule \"{}\" lists a member that does not exist: \"{member}\"",
                rule.display()
            ))
            .hint(
                "Paths in members.explicit are relative to the rule file's directory. \
                 Correct the path or remove it from the list.",
            ),

            RuleError::MemberUnlisted { rule, member } => Problem::new(format!(
                "rule \"{}\" matches a member its explicit list leaves out: \"{member}\"",
                rule.display()
            ))
            .hint(
                "Add the file to members.explicit, or keep it out of the rule with \
                 members.exclude.",
            ),

            // Past tense: a volume mounted again clears this without the
            // rule changing.
            RuleError::MemberRead { path, source } => {
                Problem::new(format!("could not read member \"{}\"", path.display()))
                    .detail(format!("Reading it failed: {source}."))
                    .hint("Check that the volume holding the rule's files is mounted and readable.")
                    .retryable(true)
            }

            RuleError::Units { rule, detail } => {
                Problem::new(format!("cannot split \"{}\" into units", rule.display()))
                    .detail(format!("Unit splitting stopped: {detail}."))
                    .hint(
                        "Put every line of prose after a line matching units.marker, or \
                         adjust units.marker and members.frontmatter.",
                    )
            }

            RuleError::Completeness {
                rule,
                gaps,
                duplicates,
                ordered,
            } => Problem::new(format!(
                "units of \"{}\" are not contiguous",
                rule.display()
            ))
            .detail(format!(
                "Completeness check failed ({}).",
                completeness_detail(gaps, duplicates, *ordered)
            ))
            .hint(
                "Fix the missing, repeated, or out-of-order units, or set \
                 completeness.fail_on_gap = false when the gaps are expected.",
            ),

            RuleError::EmptyExtraction { rule } => Problem::new(format!(
                "extraction of \"{}\" produced no body blocks",
                rule.display()
            ))
            .hint(
                "Check that the rule's member files hold prose lines after frontmatter \
                 and headings.",
            ),
        }
    }
}
