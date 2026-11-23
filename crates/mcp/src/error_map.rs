// SPDX-License-Identifier: Apache-2.0

//! Move a judged failure onto the MCP wire envelope.
//!
//! This is the crate's outbound boundary: every error an agent client
//! sees leaves through one of these helpers. Nothing is judged or
//! worded here. A tool's failure arrives judged — by the ops method
//! dispatcher, or by a session tool — and the control plane's error map
//! turns it into the code and three-part message both transports
//! answer with: the summary in `message`, the detail / hint / retryable
//! triple in `data`. This file moves that envelope across to rmcp's
//! type, so one failure takes one code and one wording on either
//! surface. `scripts/error-boundary-check.sh` keeps every MCP error
//! envelope in this file.

use bookrack_ops::methods::dispatch::DispatchError;
use bookrack_runtime::control::error_map::{dispatch_err, session_err};
use bookrack_runtime::control::jsonrpc::RpcError;
use bookrack_runtime::control::methods::session::SessionError;
use rmcp::ErrorData;
use rmcp::model::ErrorCode;

/// Map a library method's failure onto the MCP envelope.
pub(crate) fn dispatch_error_to_mcp(e: DispatchError) -> ErrorData {
    envelope(dispatch_err(e))
}

/// Map a session tool's failure onto the MCP envelope.
pub(crate) fn session_error_to_mcp(e: SessionError) -> ErrorData {
    envelope(session_err(e))
}

/// The one constructor of the MCP error envelope: the control plane's
/// envelope, code, message, and data as they are.
fn envelope(rpc: RpcError) -> ErrorData {
    ErrorData::new(ErrorCode(rpc.code), rpc.message, rpc.data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::CatalogError;
    use bookrack_embed::EmbedError;
    use bookrack_ops::OpsError;
    use bookrack_ops::error_class;
    use bookrack_ops::reads::reference::ReferenceError;
    use bookrack_ops::registry::RegistryError;
    use bookrack_query::QueryError;
    use bookrack_runtime::control::jsonrpc::{
        BACKEND_UNAVAILABLE, INVALID_LIBRARY, STATE_UNUSABLE,
    };

    /// The MCP envelope of an ops error as the dispatcher judges it.
    fn ops_error_to_mcp(e: OpsError) -> ErrorData {
        dispatch_error_to_mcp(DispatchError::Judged(error_class::from_ops(&e)))
    }

    /// The MCP envelope of a registry refusal as the dispatcher judges
    /// it.
    fn registry_err(e: &RegistryError) -> ErrorData {
        dispatch_error_to_mcp(DispatchError::Judged(error_class::from_registry(e)))
    }

    /// The MCP envelope of a reference failure as the dispatcher judges
    /// it.
    fn reference_error_to_mcp(e: ReferenceError) -> ErrorData {
        dispatch_error_to_mcp(DispatchError::Judged(error_class::from_reference(&e)))
    }

    /// The problem an envelope's `data` carries.
    fn problem_data(err: &ErrorData) -> bookrack_core::ProblemData {
        serde_json::from_value(err.data.clone().expect("data slot filled")).expect("ProblemData")
    }

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
        let data = problem_data(&err);
        assert!(data.hint.expect("hint").contains("ollama pull test-model"));
        assert!(!data.retryable);
        assert!(err.message.contains("test-model"), "{}", err.message);
    }

    /// A reranker that answered it is overloaded takes the same backend
    /// code over MCP as over the control socket, and says the call may
    /// be retried.
    #[test]
    fn an_overloaded_reranker_is_backend_unavailable_and_retryable() {
        let err = ops_error_to_mcp(OpsError::Rerank(bookrack_rerank::RerankError::Overloaded {
            status: 503,
            body: "loading model".into(),
        }));
        assert_eq!(err.code, ErrorCode(BACKEND_UNAVAILABLE), "{}", err.message);
        assert!(problem_data(&err).retryable);
    }

    /// An Ollama that answered a search's own embed round trip with
    /// "overloaded" takes the backend code over MCP, retryable, worded
    /// by the embed leaf rather than by the wrappers it came up through.
    #[test]
    fn an_overloaded_embedder_during_a_search_is_backend_unavailable_and_retryable() {
        use bookrack_search::SearchError;
        let err = ops_error_to_mcp(OpsError::Query(QueryError::Search(SearchError::Embed(
            EmbedError::Overloaded {
                status: 503,
                body: "no runner slots are available".into(),
            },
        ))));
        assert_eq!(err.code, ErrorCode(BACKEND_UNAVAILABLE), "{}", err.message);
        assert!(!err.message.contains("search error"), "{}", err.message);
        assert!(problem_data(&err).retryable);
    }

    /// An empty search query is the agent's own input: the parameter
    /// code over MCP, with the remedy in the hint.
    #[test]
    fn an_empty_search_query_is_invalid_params_with_a_hint() {
        use bookrack_search::SearchError;
        let err = ops_error_to_mcp(OpsError::Query(QueryError::Search(SearchError::EmptyQuery)));
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS, "{}", err.message);
        let data = problem_data(&err);
        assert!(data.hint.is_some_and(|h| !h.trim().is_empty()));
        assert!(!data.retryable);
    }

    #[test]
    fn a_store_this_build_cannot_serve_is_state_unusable_with_a_hint() {
        let e = OpsError::Catalog(CatalogError::SchemaTooNew {
            found: 99,
            expected: 3,
        });
        let err = ops_error_to_mcp(e);
        assert_eq!(err.code, ErrorCode(STATE_UNUSABLE), "{}", err.message);
        let hint = problem_data(&err).hint;
        assert!(
            hint.as_deref()
                .unwrap_or("")
                .contains("newer bookrack build"),
            "hint: {hint:?}"
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

    /// An unnamed tool call the daemon cannot route names a library the
    /// agent has to choose: the invalid-library code the control plane
    /// answers with, with the `library` argument as the next step in
    /// the data slot.
    #[test]
    fn an_unroutable_unnamed_call_is_invalid_library_with_a_hint() {
        let err = registry_err(&RegistryError::UnnamedAmbiguous {
            mounted: vec!["alpha".into(), "beta".into()],
            default: Some("gamma".into()),
        });
        assert_eq!(err.code, ErrorCode(INVALID_LIBRARY), "{}", err.message);
        let data = problem_data(&err);
        assert!(
            data.hint
                .as_deref()
                .unwrap_or("")
                .contains("`library` argument"),
            "hint: {:?}",
            data.hint
        );
        assert!(
            data.detail.as_deref().unwrap_or("").contains("gamma"),
            "detail: {:?}",
            data.detail
        );
    }

    /// Every registry refusal takes the code the control plane gives it:
    /// a library the agent named or must name is invalid library, an
    /// empty registry is the caller's input, and a poisoned lock is a
    /// fault.
    #[test]
    fn registry_refusals_take_the_control_planes_codes() {
        for e in [
            RegistryError::LibraryUnknown {
                name: "ghost".into(),
                available: vec!["alpha".into()],
            },
            RegistryError::UnnamedAmbiguous {
                mounted: vec!["alpha".into(), "beta".into()],
                default: None,
            },
            RegistryError::LibraryUnmounted {
                name: "beta".into(),
                reason: Box::new(bookrack_core::Problem::new("cannot read the catalog")),
            },
        ] {
            let err = registry_err(&e);
            assert_eq!(err.code, ErrorCode(INVALID_LIBRARY), "{}", err.message);
        }
        let empty = registry_err(&RegistryError::Empty);
        assert_eq!(empty.code, ErrorCode::INVALID_PARAMS, "{}", empty.message);
        for poisoned in [
            RegistryError::DefaultLockPoisoned,
            RegistryError::LibsLockPoisoned,
        ] {
            let err = registry_err(&poisoned);
            assert_eq!(err.code, ErrorCode::INTERNAL_ERROR, "{}", err.message);
        }
    }

    /// The reference tools' failures: caller input, a store this build
    /// cannot serve, and a residual fault each keep their own code.
    #[test]
    fn reference_errors_split_into_caller_input_state_and_fault() {
        let invalid = [
            ReferenceError::InvalidArgument("bad".to_owned()),
            ReferenceError::UnknownOverlayProperty {
                key: "bogus".to_owned(),
            },
        ];
        for err in invalid {
            let mapped = reference_error_to_mcp(err);
            assert_eq!(mapped.code, ErrorCode::INVALID_PARAMS, "{}", mapped.message);
        }
        let newer = reference_error_to_mcp(ReferenceError::Open {
            path: "/data/reference.db".into(),
            source: bookrack_refs::RefsError::SchemaTooNew {
                found: 99,
                supported: 1,
            },
        });
        assert_eq!(newer.code, ErrorCode(STATE_UNUSABLE), "{}", newer.message);
        let fault = reference_error_to_mcp(ReferenceError::Io {
            path: "/data/reference.db".into(),
            source: std::io::Error::other("permission denied"),
        });
        assert_eq!(fault.code, ErrorCode::INTERNAL_ERROR, "{}", fault.message);
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
}
