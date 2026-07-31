// SPDX-License-Identifier: Apache-2.0

//! The three-part diagnostic every front end renders.
//!
//! Modelled on PostgreSQL's primary / detail / hint split. A flat
//! string has to serve four consumers at once — the CLI's one-line
//! report, `--json`, the desktop shell, an agent over MCP — and ends
//! up serving none of them: it is either too terse to act on or too
//! noisy to lay out. Splitting the message lets each consumer take
//! the parts it can show.
//!
//! - `summary` is one line stating what failed. No module, function,
//!   or variant names; lowercase, no trailing period. Present tense
//!   (`cannot`) for a permanent failure, past tense (`could not`) for
//!   one that may clear on its own.
//! - `detail` carries the implementation-level evidence: HTTP status
//!   and body, a path, a syscall. Complete sentences, so a verbose
//!   mode can drop it without the summary losing meaning.
//! - `hint` says what to do next. Complete sentences. It is allowed
//!   to be wrong — being its own field is exactly what lets it hold a
//!   guess without weakening the summary's factual claim.
//!
//! [`Problem`] is a presentation type, not an error: it deliberately
//! does **not** implement [`std::error::Error`], so it cannot be
//! returned in place of one or folded into a cause chain.

use std::error::Error;

use serde::{Deserialize, Serialize};

use crate::error_chain;

/// The parts of a [`Problem`] below the summary line.
///
/// Split out from `Problem` so the control plane can put exactly
/// these three keys in a JSON-RPC error's `data` slot while the
/// summary stays in `message`, and so a client can read them back
/// with one `from_value` instead of picking keys by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct ProblemData {
    /// Implementation-level evidence for the summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

    /// What the operator should do next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,

    /// Whether the same call, resent unchanged, may succeed without
    /// anyone intervening.
    ///
    /// This is the field an agent branches on, so it must not be
    /// inferred from the wording of `summary`. It is *not* the same
    /// question as `EmbedError::is_transient()`, which asks whether
    /// the client should retry the identical batch transparently: an
    /// overloaded server answers `true` here (its state is momentary)
    /// and `false` there (resending the same batch is what overloaded
    /// it). The two are expected to disagree; neither is out of sync
    /// with the other.
    pub retryable: bool,
}

/// A rendered, three-part diagnostic.
///
/// Serializes as four flat fields — `summary` plus [`ProblemData`]'s
/// three — so `--json` output and the wire `data` object share one
/// shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "./"))]
pub struct Problem {
    /// One line naming what failed.
    pub summary: String,

    /// The detail / hint / retryable triple.
    #[serde(flatten)]
    pub data: ProblemData,
}

impl Problem {
    /// Start a problem with only its summary line. `detail` and `hint`
    /// stay empty and `retryable` stays `false` until a builder call
    /// sets them.
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            data: ProblemData {
                detail: None,
                hint: None,
                retryable: false,
            },
        }
    }

    /// Attach the implementation-level evidence.
    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.data.detail = Some(detail.into());
        self
    }

    /// Attach the next step.
    #[must_use]
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.data.hint = Some(hint.into());
        self
    }

    /// Declare whether resending the same call unchanged may succeed.
    #[must_use]
    pub fn retryable(mut self, retryable: bool) -> Self {
        self.data.retryable = retryable;
        self
    }

    /// Fall back to a two-part problem derived from an error value:
    /// the outermost `Display` becomes the summary, the flattened
    /// source chain becomes the detail.
    ///
    /// `retryable` is `false`, because an error type that has not
    /// opted into [`Explain`] has not said whether retrying helps and
    /// a wrong `true` sends a client into a loop no one is watching.
    pub fn from_error_chain(err: &(dyn Error + 'static)) -> Self {
        Self::new(err.to_string()).detail(error_chain(err))
    }
}

/// An error type that knows how to present itself to an operator.
///
/// Implementing this is opt-in: a type that does not gets
/// [`Problem::from_error_chain`] applied at the boundary, which is
/// strictly better than the flat `Display` that preceded it.
pub trait Explain {
    /// Render this error as a three-part diagnostic.
    fn explain(&self) -> Problem;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Leaf;

    impl std::fmt::Display for Leaf {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("connection refused")
        }
    }

    impl Error for Leaf {}

    #[derive(Debug)]
    struct Wrapper(Leaf);

    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("embed error")
        }
    }

    impl Error for Wrapper {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn from_error_chain_puts_the_flattened_chain_in_detail() {
        let p = Problem::from_error_chain(&Wrapper(Leaf));
        assert_eq!(p.summary, "embed error");
        assert_eq!(
            p.data.detail.as_deref(),
            Some("embed error: connection refused")
        );
    }

    #[test]
    fn from_error_chain_defaults_to_not_retryable() {
        let p = Problem::from_error_chain(&Wrapper(Leaf));
        assert!(!p.data.retryable);
        assert!(p.data.hint.is_none());
    }

    #[test]
    fn a_problem_serializes_as_four_flat_fields() {
        let p = Problem::new("cannot embed")
            .detail("HTTP 404.")
            .hint("Pull it first.")
            .retryable(false);
        let v = serde_json::to_value(&p).expect("serialize");
        assert_eq!(
            v,
            serde_json::json!({
                "summary": "cannot embed",
                "detail": "HTTP 404.",
                "hint": "Pull it first.",
                "retryable": false,
            })
        );
    }

    #[test]
    fn empty_parts_are_omitted_but_retryable_is_always_present() {
        let v = serde_json::to_value(Problem::new("could not reach Ollama").retryable(true))
            .expect("serialize");
        assert_eq!(
            v,
            serde_json::json!({ "summary": "could not reach Ollama", "retryable": true })
        );
    }

    /// The wire `data` object is a serialized [`ProblemData`], so a
    /// client must be able to read one back from exactly the keys the
    /// server emitted.
    #[test]
    fn problem_data_round_trips_through_the_wire_shape() {
        let p = Problem::new("cannot embed")
            .detail("HTTP 404.")
            .hint("Pull it first.");
        let wire = serde_json::to_value(&p.data).expect("serialize");
        let back: ProblemData = serde_json::from_value(wire).expect("deserialize");
        assert_eq!(back, p.data);
    }
}
