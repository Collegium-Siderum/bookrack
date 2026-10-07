// SPDX-License-Identifier: Apache-2.0

//! Map typed server-side errors onto the MCP wire envelope.
//!
//! This is the crate's outbound boundary: every message an agent
//! client sees leaves through one of these helpers. Wording is not
//! written here — each error renders itself through
//! [`bookrack_core::Explain`], and [`mcp_from_problem`] splits the
//! result across the MCP envelope: the summary into `message`, the
//! detail / hint / retryable triple into `data`. A type with no
//! `Explain` impl takes [`Problem::from_error_chain`], whose summary
//! is the flattened cause chain — `Display` on a wrapper variant
//! prints only its own text and would drop the root cause exactly
//! where the caller can no longer reach it.
//! `scripts/error-boundary-check.sh` enforces that for this file.

use bookrack_core::{Explain, Problem};
use bookrack_ops::OpsError;
use bookrack_ops::dto::UnknownFilterValue;
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock, ErrorCode};
use serde::Serialize;

use crate::reference;
use crate::translate::TranslateToolError;

/// Encode `value` to a JSON string and wrap it as the body of a successful
/// tool response. Centralises serialization so every tool returns the same
/// `text` content shape.
pub(crate) fn respond_with<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let json = serde_json::to_string(value)
        .map_err(|e| mcp_from_problem(ErrorCode::INTERNAL_ERROR, Problem::from_error_chain(&e)))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
}

/// Build the MCP error envelope from a rendered [`Problem`]: the
/// summary line becomes `message`, the other three parts become
/// `data`. Mirrors the control plane's `rpc_from_problem`, so the two
/// front ends put the same object in the same slot.
///
/// `message` stays self-sufficient on its own, so a client that reads
/// only that field learns no less than it did before `data` existed.
pub(crate) fn mcp_from_problem(code: ErrorCode, problem: Problem) -> ErrorData {
    ErrorData::new(
        code,
        problem.summary,
        serde_json::to_value(problem.data).ok(),
    )
}

/// Map a validation or argument-shape error to an MCP `invalid_params`.
///
/// The tool bodies reject caller input in a handful of places that hold
/// no typed enum worth matching on (exclusion-list validation, registry
/// lookup, single-variant arms). They all funnel here so the outbound
/// message stays flattened and this file remains the crate's only
/// unguarded exit.
pub(crate) fn invalid_params_err<E: std::error::Error + 'static>(e: &E) -> ErrorData {
    mcp_from_problem(ErrorCode::INVALID_PARAMS, Problem::from_error_chain(e))
}

/// Map a refused filter value to an MCP `invalid_params`.
///
/// The accepted set travels with the refusal rather than being spelled
/// out here, so a vocabulary that gains a value cannot leave a stale
/// list behind on this surface.
pub(crate) fn unknown_filter_value_to_mcp(unknown: &UnknownFilterValue) -> ErrorData {
    mcp_from_problem(
        ErrorCode::INVALID_PARAMS,
        Problem::new(format!(
            "cannot filter on {} value {:?}",
            unknown.parameter, unknown.value
        ))
        .detail(format!(
            "The {} filter names a value no row carries, so it was refused rather \
             than applied without it.",
            unknown.parameter
        ))
        .hint(format!("Use one of: {}.", unknown.accepted.join(", "))),
    )
}

/// Map an [`OpsError`] onto the MCP envelope through the control
/// plane's classifier.
///
/// [`bookrack_runtime::control::error_map::ops_err`] decides the code
/// — caller input, a store this build cannot serve, a backend that
/// did not answer, or a residual internal fault — and renders the
/// three-part message; this function only moves the envelope across
/// to rmcp's type. The code is not re-decided here, so a typed error
/// takes the same code over MCP as over the control socket.
///
/// Tool bodies that answer a missing id with a `null` body match that
/// variant before reaching this function; everything else funnels
/// here.
pub(crate) fn ops_error_to_mcp(e: OpsError) -> ErrorData {
    let rpc = bookrack_runtime::control::error_map::ops_err(e);
    ErrorData::new(ErrorCode(rpc.code), rpc.message, rpc.data)
}

/// Map a [`reference::ReferenceError`] to an MCP error: the
/// catalog / argument-shape variants are caller-input problems and
/// surface as `invalid_params`, the refs-store and catalog-load
/// variants are environmental and surface as `internal_error`.
pub(crate) fn reference_error_to_mcp(e: reference::ReferenceError) -> ErrorData {
    match e {
        reference::ReferenceError::InvalidArgument(_)
        | reference::ReferenceError::UnknownOverlayProperty { .. } => invalid_params_err(&e),
        reference::ReferenceError::Refs(_) | reference::ReferenceError::Catalog(_) => {
            mcp_from_problem(ErrorCode::INTERNAL_ERROR, Problem::from_error_chain(&e))
        }
    }
}

/// Map a [`TranslateToolError`] onto the MCP envelope. Caller input
/// — an argument that describes no query, an unknown injection
/// profile — is `invalid_params`; a translation store this build
/// cannot serve is the control plane's state-unusable code, as it is
/// for the catalog and corpus; everything else, including a corpus or
/// reference store that failed to open and a drifted source text, is
/// `internal_error`. Wording comes from the error's own [`Explain`].
pub(crate) fn translate_error_to_mcp(e: TranslateToolError) -> ErrorData {
    use bookrack_runtime::control::jsonrpc::STATE_UNUSABLE;
    use bookrack_translate::TranslateError;
    let code = match &e {
        // A write refused against the segment's current state is the
        // caller's to correct by fetching again; the store itself is
        // fine, so none of these is a state-unusable code.
        TranslateToolError::InvalidArgument(_)
        | TranslateToolError::Translate(
            TranslateError::UnknownProfile { .. }
            | TranslateError::VersionConflict { .. }
            | TranslateError::WrongStatus { .. }
            | TranslateError::NotVirgin { .. }
            | TranslateError::NotEmpty { .. }
            | TranslateError::MissingField { .. }
            | TranslateError::UnknownSegment { .. }
            | TranslateError::UnknownUnit { .. }
            | TranslateError::UnknownTerm { .. }
            | TranslateError::UnknownTranslation { .. }
            | TranslateError::UnknownValue { .. },
        ) => ErrorCode::INVALID_PARAMS,
        TranslateToolError::Translate(
            TranslateError::SchemaTooNew { .. }
            | TranslateError::SchemaTooOld { .. }
            | TranslateError::ReaderTooOld { .. }
            | TranslateError::Verify(_),
        ) => ErrorCode(STATE_UNUSABLE),
        TranslateToolError::Translate(TranslateError::Sqlite(_) | TranslateError::Migrate(_))
        | TranslateToolError::Corpus(_)
        | TranslateToolError::Refs(_)
        | TranslateToolError::Reference(_)
        | TranslateToolError::SourceDrift { .. }
        | TranslateToolError::BrokenUnitLink { .. } => ErrorCode::INTERNAL_ERROR,
    };
    mcp_from_problem(code, e.explain())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::CatalogError;
    use bookrack_embed::EmbedError;
    use bookrack_query::QueryError;
    use bookrack_runtime::control::jsonrpc::STATE_UNUSABLE;

    #[test]
    fn wrapper_error_keeps_its_root_cause_on_the_wire() {
        let e = OpsError::Query(QueryError::Embed(EmbedError::Unreachable("boom".into())));
        let data = ops_error_to_mcp(e);
        let wire = serde_json::to_string(&data).expect("serialize");
        assert!(wire.contains("boom"), "root cause lost: {wire}");
    }

    #[test]
    fn mcp_error_carries_detail_and_hint_in_data() {
        let e = OpsError::Query(QueryError::Embed(EmbedError::ModelNotFound {
            model: "test-model".into(),
            reason: "model not found".into(),
        }));
        let err = ops_error_to_mcp(e);
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{}", err.message);
        let data: bookrack_core::ProblemData =
            serde_json::from_value(err.data.expect("data slot filled")).expect("ProblemData");
        assert!(data.hint.expect("hint").contains("ollama pull test-model"));
        assert!(!data.retryable);
        assert!(err.message.contains("test-model"), "{}", err.message);
    }

    #[test]
    fn a_store_this_build_cannot_serve_is_state_unusable_with_a_hint() {
        let e = OpsError::Catalog(CatalogError::SchemaTooNew {
            found: 99,
            expected: 3,
        });
        let err = ops_error_to_mcp(e);
        assert_eq!(err.code, ErrorCode(STATE_UNUSABLE), "{}", err.message);
        let data: bookrack_core::ProblemData =
            serde_json::from_value(err.data.expect("data slot filled")).expect("ProblemData");
        assert!(
            data.hint
                .as_deref()
                .unwrap_or("")
                .contains("newer bookrack build"),
            "hint: {:?}",
            data.hint
        );
    }

    #[test]
    fn caller_input_variants_are_invalid_params_and_the_rest_internal() {
        for err in [
            OpsError::UnknownMetadataField {
                field: "bogus".into(),
                editable: vec![String::from("title")],
            },
            OpsError::UnknownContributorRole {
                role: "bogus".into(),
            },
            OpsError::ContributorNotFound {
                contributor_id: 7,
                intake_id: 1,
            },
            OpsError::IntakeNotFound { intake_id: 42 },
        ] {
            let mapped = ops_error_to_mcp(err);
            assert_eq!(mapped.code, ErrorCode::INVALID_PARAMS, "{}", mapped.message);
        }
        let mapped = ops_error_to_mcp(OpsError::SearchUnavailable);
        assert_eq!(mapped.code, ErrorCode::INTERNAL_ERROR, "{}", mapped.message);
    }

    #[test]
    fn user_input_error_message_is_unchanged_by_flattening() {
        let e = OpsError::UnknownMetadataField {
            field: "no_such_field".into(),
            editable: vec![String::from("title"), String::from("year")],
        };
        let expected = e.to_string(); // error-boundary-check: allow
        let data = ops_error_to_mcp(e);
        assert_eq!(data.message, expected);
    }

    #[test]
    fn translate_errors_take_the_code_their_class_prescribes() {
        use bookrack_translate::TranslateError;
        let invalid = translate_error_to_mcp(TranslateToolError::InvalidArgument(
            "`text` is empty".into(),
        ));
        assert_eq!(
            invalid.code,
            ErrorCode::INVALID_PARAMS,
            "{}",
            invalid.message
        );
        assert!(
            invalid.message.contains("`text` is empty"),
            "{}",
            invalid.message
        );

        let stale = translate_error_to_mcp(TranslateToolError::Translate(
            TranslateError::VersionConflict {
                segment_id: 7,
                expected: 1,
                current: 2,
            },
        ));
        assert_eq!(stale.code, ErrorCode::INVALID_PARAMS, "{}", stale.message);
        assert!(stale.message.contains("segment 7"), "{}", stale.message);

        let profile = translate_error_to_mcp(TranslateToolError::Translate(
            TranslateError::UnknownProfile {
                name: "verbose".into(),
            },
        ));
        assert_eq!(
            profile.code,
            ErrorCode::INVALID_PARAMS,
            "{}",
            profile.message
        );
        assert!(profile.message.contains("verbose"), "{}", profile.message);

        let unusable = translate_error_to_mcp(TranslateToolError::Translate(
            TranslateError::SchemaTooNew {
                found: 9,
                expected: 1,
            },
        ));
        assert_eq!(
            unusable.code,
            ErrorCode(STATE_UNUSABLE),
            "{}",
            unusable.message
        );
        let data: bookrack_core::ProblemData =
            serde_json::from_value(unusable.data.expect("data slot filled")).expect("ProblemData");
        assert!(data.hint.is_some(), "a refused store names the way out");

        let drift = translate_error_to_mcp(TranslateToolError::SourceDrift { segment_id: 7 });
        assert_eq!(drift.code, ErrorCode::INTERNAL_ERROR, "{}", drift.message);
        assert!(drift.message.contains("segment 7"), "{}", drift.message);

        let corpus = translate_error_to_mcp(TranslateToolError::Corpus(
            bookrack_corpus::CorpusError::InvalidLeafRun {
                start: 1,
                end: 2,
                reason: "end precedes start",
            },
        ));
        assert_eq!(corpus.code, ErrorCode::INTERNAL_ERROR, "{}", corpus.message);
        assert!(
            corpus.message.contains("end precedes start"),
            "root cause lost: {}",
            corpus.message
        );
    }
}
