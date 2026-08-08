// SPDX-License-Identifier: Apache-2.0

//! Operator input a write command refused.
//!
//! The `cmd::*` layer returns [`eyre::Report`], so a refusal written as
//! `bail!` or `with_context` reaches the control plane as an untyped
//! error and is classified as a handler-side fault. [`CmdInputError`]
//! is the type a write command raises instead: the control plane's
//! `write_err` recognises it in the cause chain and maps it onto the
//! caller-input codes.
//!
//! Every variant has a next step the caller can take — that is the
//! test for whether a refusal belongs here at all. A failure with no
//! next step to write is a fault and stays untyped.
//!
//! Each variant carries only what its construction site knows and the
//! type cannot derive: the id that was not found, the value that was
//! rejected, the set it was measured against. Wording is assembled in
//! the [`Explain`] impl, which is the single place the three-part
//! discipline is applied.

use bookrack_core::{Explain, Problem};

/// Operator input a write command refused. Distinct from a fault:
/// every variant has a next step the caller can take.
///
/// All variants map onto `-32602 invalid params` except
/// [`CmdInputError::TargetDrifted`], which has its own code because a
/// client recovers from it differently — by minting a fresh plan
/// rather than by correcting a parameter.
#[derive(Debug, thiserror::Error)]
pub enum CmdInputError {
    /// The catalog holds no intake under the id the caller named.
    #[error("no intake registered for book {intake_id}")]
    UnknownIntake {
        /// The intake id the caller asked for.
        intake_id: i64,
    },

    /// The catalog holds no intake under the source hash the caller
    /// named.
    #[error("no intake registered for source_sha256 {sha:?}")]
    UnknownSha {
        /// The source hash the caller asked for.
        sha: String,
    },

    /// The library has not been built up to the layer this command
    /// reads. The library exists; the layer does not yet.
    #[error("library has no {what} yet")]
    NotIngested {
        /// The absent layer, named as an operator would name it
        /// (`"catalog"`, `"ingested chunks"`).
        what: &'static str,
        /// The pipeline step that produces `what`.
        hint: &'static str,
    },

    /// A parameter carried a value outside its closed set.
    #[error("{arg} {value:?} is not a supported value")]
    BadArgument {
        /// The wire name of the parameter.
        arg: &'static str,
        /// The value the caller passed.
        value: String,
        /// The full set the value is measured against, already
        /// rendered for display. Derive it from the set's own
        /// definition rather than transcribing it, so a new member
        /// cannot drift out of this message.
        expected: String,
    },

    /// The input is well-formed and addresses something real, but
    /// there is nothing for the command to act on.
    #[error("{summary}")]
    NothingToDo {
        /// One line naming what was found empty.
        summary: String,
        /// What would give the command something to act on.
        hint: String,
    },

    /// A precondition the command checks before writing is not met.
    #[error("{summary}")]
    Refused {
        /// One line naming the precondition that failed.
        summary: String,
        /// What to change, when the summary does not already imply it.
        hint: Option<String>,
    },

    /// The target moved between the dry-run leg and the execute leg,
    /// so the plan the caller confirmed no longer describes what the
    /// command would do.
    #[error("plan target for book {intake_id} no longer matches the catalog")]
    TargetDrifted {
        /// The intake the plan was minted against.
        intake_id: i64,
        /// How the target moved.
        detail: String,
    },
}

/// The next step for both "the selector resolved to nothing" variants.
/// Shared as one constant because the two differ only in which
/// selector was used, which the summary already says.
const LIST_INTAKES: &str = "Run `bookrack list` to see the registered intakes.";

impl Explain for CmdInputError {
    fn explain(&self) -> Problem {
        // `retryable` stays at the `Problem::new` default of `false`
        // for every variant: resending an identical request cannot
        // make an unknown id known or a rejected value supported.
        // `TargetDrifted` is the one that invites a second look and it
        // is `false` too — the plan id was consumed on this call, so
        // the identical request cannot succeed; minting a fresh plan
        // is a different request, and that belongs in the hint.
        match self {
            CmdInputError::UnknownIntake { intake_id } => {
                Problem::new(format!("no intake registered for book {intake_id}"))
                    .hint(LIST_INTAKES)
            }

            CmdInputError::UnknownSha { sha } => {
                Problem::new(format!("no intake registered for source_sha256 {sha:?}"))
                    .hint(LIST_INTAKES)
            }

            CmdInputError::NotIngested { what, hint } => {
                Problem::new(format!("library has no {what} yet")).hint(*hint)
            }

            CmdInputError::BadArgument {
                arg,
                value,
                expected,
            } => Problem::new(format!("{arg} {value:?} is not a supported value"))
                .detail(format!("Supported values: {expected}.")),

            CmdInputError::NothingToDo { summary, hint } => Problem::new(summary).hint(hint),

            CmdInputError::Refused { summary, hint } => {
                let problem = Problem::new(summary);
                match hint {
                    Some(hint) => problem.hint(hint),
                    None => problem,
                }
            }

            CmdInputError::TargetDrifted { intake_id, detail } => Problem::new(format!(
                "plan target for book {intake_id} no longer matches the catalog"
            ))
            .detail(detail)
            .hint("Re-run the command with dry_run=true and confirm the fresh plan."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of each variant, so a test that walks the set cannot go
    /// stale by omission: adding a variant without extending this
    /// makes the `match` below fail to compile.
    fn one_of_each() -> Vec<CmdInputError> {
        let sample = CmdInputError::UnknownIntake { intake_id: 1 };
        // Exhaustive on purpose: a new variant must be added to the
        // list that follows, not merely acknowledged here.
        match sample {
            CmdInputError::UnknownIntake { .. }
            | CmdInputError::UnknownSha { .. }
            | CmdInputError::NotIngested { .. }
            | CmdInputError::BadArgument { .. }
            | CmdInputError::NothingToDo { .. }
            | CmdInputError::Refused { .. }
            | CmdInputError::TargetDrifted { .. } => {}
        }
        vec![
            CmdInputError::UnknownIntake { intake_id: 999_999 },
            CmdInputError::UnknownSha {
                sha: "deadbeef".into(),
            },
            CmdInputError::NotIngested {
                what: "catalog",
                hint: "Ingest a book into this library first.",
            },
            CmdInputError::BadArgument {
                arg: "kind",
                value: "nosuch".into(),
                expected: "ivf-flat, hnsw".into(),
            },
            CmdInputError::NothingToDo {
                summary: "no supported files found under \"/x\"".into(),
                hint: "Point the command at a directory holding one of the supported formats."
                    .into(),
            },
            CmdInputError::Refused {
                summary: "library name is empty".into(),
                hint: None,
            },
            CmdInputError::TargetDrifted {
                intake_id: 7,
                detail: "The intake was removed after the plan was minted.".into(),
            },
        ]
    }

    /// The type's stated contract: resending an identical request
    /// cannot clear any of these. A wrong `true` here sends an agent
    /// into a loop no one is watching.
    #[test]
    fn no_variant_is_retryable() {
        for e in one_of_each() {
            assert!(
                !e.explain().data.retryable,
                "{e:?} claims resending it unchanged may succeed"
            );
        }
    }

    /// The summary rules from the error discipline, checked
    /// mechanically: one line, lowercase opening, no trailing period.
    #[test]
    fn every_summary_obeys_the_shape_rules() {
        for e in one_of_each() {
            let summary = e.explain().summary;
            assert!(
                !summary.contains('\n'),
                "a summary is one line: {summary:?}"
            );
            assert!(
                !summary.ends_with('.'),
                "a summary carries no trailing period: {summary:?}"
            );
            let first = summary.chars().next().expect("non-empty summary");
            assert!(
                !first.is_uppercase(),
                "a summary opens lowercase: {summary:?}"
            );
        }
    }

    /// String selectors are quoted, numeric ids are not — the same
    /// split `OpsError::IntakeNotFound` already presents.
    #[test]
    fn string_selectors_are_quoted_and_numeric_ids_are_not() {
        let numeric = CmdInputError::UnknownIntake { intake_id: 999_999 }
            .explain()
            .summary;
        assert!(numeric.contains("book 999999"), "{numeric}");
        assert!(
            !numeric.contains('"'),
            "a numeric id takes no quotes: {numeric}"
        );

        let textual = CmdInputError::UnknownSha {
            sha: "deadbeef".into(),
        }
        .explain()
        .summary;
        assert!(textual.contains("\"deadbeef\""), "{textual}");
    }

    /// The rejected value alone does not tell a caller what to send
    /// instead; the accepted set is the evidence that does.
    #[test]
    fn bad_argument_puts_the_accepted_set_in_detail() {
        let problem = CmdInputError::BadArgument {
            arg: "kind",
            value: "nosuch".into(),
            expected: "ivf-flat, hnsw".into(),
        }
        .explain();
        assert!(problem.summary.contains("\"nosuch\""), "{problem:?}");
        let detail = problem.data.detail.expect("detail names the accepted set");
        assert!(detail.contains("ivf-flat"), "{detail}");
        assert!(detail.contains("hnsw"), "{detail}");
    }

    /// The drift legs each report how the target moved; that evidence
    /// is implementation-level and belongs below the summary line.
    #[test]
    fn target_drifted_keeps_its_evidence_out_of_the_summary() {
        let problem = CmdInputError::TargetDrifted {
            intake_id: 7,
            detail: "Fingerprint a1b2 was pinned; the catalog now reads c3d4.".into(),
        }
        .explain();
        assert!(!problem.summary.contains("a1b2"), "{}", problem.summary);
        assert!(
            problem.data.detail.expect("detail").contains("a1b2"),
            "the pinned fingerprint is the evidence"
        );
        assert!(problem.data.hint.expect("hint").contains("dry_run=true"));
    }

    /// `Refused` is the one variant whose hint is optional: several of
    /// its sites state the fix in the summary itself.
    #[test]
    fn refused_renders_with_and_without_a_hint() {
        let bare = CmdInputError::Refused {
            summary: "library name is empty".into(),
            hint: None,
        }
        .explain();
        assert_eq!(bare.summary, "library name is empty");
        assert!(bare.data.hint.is_none());

        let hinted = CmdInputError::Refused {
            summary: "book 4 has no parsed_at".into(),
            hint: Some("Run the structure pass before advancing it.".into()),
        }
        .explain();
        assert!(hinted.data.hint.expect("hint").contains("structure pass"));
    }
}
