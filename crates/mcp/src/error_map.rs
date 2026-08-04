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

use bookrack_catalog::IntakeStatus;
use bookrack_core::{Explain, Problem};
use bookrack_ops::OpsError;
use bookrack_ops::dto::UnknownStatus;
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, Content, ErrorCode};
use serde::Serialize;

use crate::reference;

/// Encode `value` to a JSON string and wrap it as the body of a successful
/// tool response. Centralises serialization so every tool returns the same
/// `text` content shape.
pub(crate) fn respond_with<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let json = serde_json::to_string(value)
        .map_err(|e| mcp_from_problem(ErrorCode::INTERNAL_ERROR, Problem::from_error_chain(&e)))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
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

/// Map a rejected lifecycle-status name to an MCP `invalid_params`.
///
/// The accepted set is rendered from [`IntakeStatus::ALL`] rather than
/// spelled out, so a state added to the lifecycle cannot leave a stale
/// list behind on this surface.
pub(crate) fn unknown_status_to_mcp(tool: &str, unknown: &UnknownStatus) -> ErrorData {
    let accepted = IntakeStatus::ALL
        .iter()
        .map(|status| status.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    mcp_from_problem(
        ErrorCode::INVALID_PARAMS,
        Problem::new(format!(
            "cannot filter on lifecycle status \"{}\"",
            unknown.0
        ))
        .detail(format!(
            "{tool} received a status no book can be in, so the filter was refused \
                 rather than applied without it."
        ))
        .hint(format!("Use one of: {accepted}.")),
    )
}

/// Map a generic [`OpsError`] to an MCP internal error.
pub(crate) fn ops_error_to_internal(e: OpsError) -> ErrorData {
    mcp_from_problem(ErrorCode::INTERNAL_ERROR, e.explain())
}

/// Map an [`OpsError`] from a metadata field edit to an MCP error:
/// a rejected field name is the caller's input problem, so it surfaces
/// as `invalid_params` (with the editable list in the message) rather
/// than an internal error.
pub(crate) fn ops_error_to_edit_error(e: OpsError) -> ErrorData {
    match &e {
        OpsError::UnknownMetadataField { .. }
        | OpsError::UnknownContributorRole { .. }
        | OpsError::ContributorNotFound { .. } => {
            mcp_from_problem(ErrorCode::INVALID_PARAMS, e.explain())
        }
        _ => ops_error_to_internal(e),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_embed::EmbedError;
    use bookrack_query::QueryError;

    #[test]
    fn wrapper_error_keeps_its_root_cause_on_the_wire() {
        let e = OpsError::Query(QueryError::Embed(EmbedError::Unreachable("boom".into())));
        let data = ops_error_to_internal(e);
        let wire = serde_json::to_string(&data).expect("serialize");
        assert!(wire.contains("boom"), "root cause lost: {wire}");
    }

    #[test]
    fn mcp_error_carries_detail_and_hint_in_data() {
        let e = OpsError::Query(QueryError::Embed(EmbedError::ModelNotFound {
            model: "test-model".into(),
            reason: "model not found".into(),
        }));
        let err = ops_error_to_internal(e);
        let data: bookrack_core::ProblemData =
            serde_json::from_value(err.data.expect("data slot filled")).expect("ProblemData");
        assert!(data.hint.expect("hint").contains("ollama pull test-model"));
        assert!(!data.retryable);
        assert!(err.message.contains("test-model"), "{}", err.message);
    }

    #[test]
    fn user_input_error_message_is_unchanged_by_flattening() {
        let e = OpsError::UnknownMetadataField {
            field: "no_such_field".into(),
        };
        let expected = e.to_string(); // error-boundary-check: allow
        let data = ops_error_to_edit_error(e);
        assert_eq!(data.message, expected);
    }
}
