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
        }
    }
}
