// SPDX-License-Identifier: Apache-2.0

//! Map typed server-side errors onto the MCP wire envelope.
//!
//! This is the crate's outbound boundary: every message an agent
//! client sees leaves through one of these helpers. Messages are
//! flattened with [`bookrack_core::error_chain`] rather than
//! `Display`ed, because `Display` on a wrapper variant prints only
//! its own text and would drop the root cause exactly where the
//! caller can no longer reach it.
//! `scripts/error-boundary-check.sh` enforces that for this file.

use bookrack_ops::OpsError;
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, Content};
use serde::Serialize;

use crate::reference;

/// Encode `value` to a JSON string and wrap it as the body of a successful
/// tool response. Centralises serialization so every tool returns the same
/// `text` content shape.
pub(crate) fn respond_with<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let json = serde_json::to_string(value)
        .map_err(|e| ErrorData::internal_error(bookrack_core::error_chain(&e), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Map a validation or argument-shape error to an MCP `invalid_params`.
///
/// The tool bodies reject caller input in a handful of places that hold
/// no typed enum worth matching on (exclusion-list validation, registry
/// lookup, single-variant arms). They all funnel here so the outbound
/// message stays flattened and this file remains the crate's only
/// unguarded exit.
pub(crate) fn invalid_params_err<E: std::error::Error + 'static>(e: &E) -> ErrorData {
    ErrorData::invalid_params(bookrack_core::error_chain(e), None)
}

/// Map a generic [`OpsError`] to an MCP internal error.
pub(crate) fn ops_error_to_internal(e: OpsError) -> ErrorData {
    ErrorData::internal_error(bookrack_core::error_chain(&e), None)
}

/// Map an [`OpsError`] from a metadata field edit to an MCP error:
/// a rejected field name is the caller's input problem, so it surfaces
/// as `invalid_params` (with the editable list in the message) rather
/// than an internal error.
pub(crate) fn ops_error_to_edit_error(e: OpsError) -> ErrorData {
    match &e {
        OpsError::UnknownMetadataField { .. }
        | OpsError::UnknownContributorRole { .. }
        | OpsError::ContributorNotFound { .. } => invalid_params_err(&e),
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
            ErrorData::internal_error(bookrack_core::error_chain(&e), None)
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
        assert!(
            data.message.contains("boom"),
            "root cause lost: {}",
            data.message
        );
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
