// SPDX-License-Identifier: Apache-2.0

//! Classify a write-handler error onto the right JSON-RPC code.
//!
//! Write-class control-plane RPCs receive `eyre::Report` from the
//! `cmd::*` layer (which folds typed downstream errors through
//! `?`/`.context()`). Reporting every such error as
//! [`INTERNAL_ERROR`] hides user-input failures — unknown intakes,
//! validation refusals, unknown libraries — from MCP/CLI clients,
//! who then cannot distinguish a caller-side input problem from a
//! genuine server fault.
//!
//! [`write_err`] walks the `eyre` cause chain looking for known typed
//! errors ([`OpsError`], [`IngestError`], [`GleanError`],
//! [`RegistryError`], [`CmdInputError`]) and, when one matches, maps a
//! user-input variant onto [`INVALID_PARAMS`] (or the bookrack-specific
//! code reserved for that shape, e.g. [`INVALID_LIBRARY`]). Anything
//! that does not match a known user-input variant falls through to
//! [`INTERNAL_ERROR`].
//!
//! The first four are raised by the layers below `cmd::*`, so a
//! refusal a write command makes on its own — before it reaches ops,
//! ingest, or glean — is only classified if the command raises
//! [`CmdInputError`] rather than a bare `bail!`.
//!
//! Wording is not written here. Each error renders itself through
//! [`bookrack_core::Explain`], and [`rpc_from_problem`] splits the
//! result across the envelope: the summary into `message`, the
//! detail / hint / retryable triple into `data`. A type with no
//! `Explain` impl takes [`Problem::from_error_chain`], whose summary
//! is the flattened cause chain — `Display` on a wrapper variant
//! prints only its own text ("query error"), so a bare `to_string()`
//! here would drop the root cause at the process boundary.
//! `scripts/error-boundary-check.sh` enforces that.

use bookrack_config::ConfigError;
use bookrack_core::{Explain, Problem};
use bookrack_glean::GleanError;
use bookrack_ingest::IngestError;
use bookrack_ops::OpsError;
use bookrack_ops::registry::RegistryError;
use eyre::Report;

use super::jsonrpc::{
    INTERNAL_ERROR, INVALID_LIBRARY, INVALID_PARAMS, PLAN_KIND_MISMATCH, PLAN_LIBRARY_MISMATCH,
    PLAN_NOT_FOUND, PLAN_TARGET_DRIFTED, RpcError,
};
use super::plan_registry::PlanLookupError;
use crate::cmd::input_error::CmdInputError;

/// Map a write-handler error onto a JSON-RPC error envelope.
///
/// `method` is the wire-name of the failing RPC (`"metadata.set"`,
/// `"corpus.rebuild"`, ...), used only to label the residual
/// [`INTERNAL_ERROR`] message.
pub(crate) fn write_err(method: &str, err: Report) -> RpcError {
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<OpsError>() {
            return from_ops(e);
        }
        if let Some(e) = cause.downcast_ref::<IngestError>() {
            return from_ingest(e);
        }
        if let Some(e) = cause.downcast_ref::<GleanError>() {
            return from_glean(e);
        }
        if let Some(e) = cause.downcast_ref::<RegistryError>() {
            return from_registry(e);
        }
        if let Some(e) = cause.downcast_ref::<CmdInputError>() {
            return from_cmd_input(e);
        }
    }
    RpcError::new(INTERNAL_ERROR, format!("{method} failed: {err:#}"))
}

/// Map a directly-held [`OpsError`] without an `anyhow` round-trip.
#[allow(dead_code)]
pub(crate) fn ops_err(e: OpsError) -> RpcError {
    from_ops(&e)
}

/// Map a directly-held [`RegistryError`] without an `anyhow` round-trip.
pub(crate) fn registry_err(e: RegistryError) -> RpcError {
    from_registry(&e)
}

/// Map a directly-held [`ConfigError`] from a registry write onto the
/// corresponding wire code. An unknown library named against the
/// on-disk registry is caller input ([`INVALID_LIBRARY`]); every other
/// registry fault is a server-side [`INTERNAL_ERROR`].
pub(crate) fn config_err(e: ConfigError) -> RpcError {
    from_config(&e)
}

/// Map a [`PlanLookupError`] onto the corresponding wire code.
///
/// The destructive RPC pin protocol surfaces three failure modes on
/// the execute leg: missing / expired ids land on
/// [`PLAN_NOT_FOUND`] (collapsed together so the wire-level
/// appearance of "I do not have this id" is consistent), and the
/// scope-violation variants land on their dedicated codes so a
/// client can distinguish operator error (wrong kind, wrong
/// library) from drift (expired or consumed).
pub(crate) fn plan_lookup_err(e: PlanLookupError) -> RpcError {
    match e {
        PlanLookupError::NotFound => RpcError::new(
            PLAN_NOT_FOUND,
            "plan_id not found: register a fresh plan with dry_run=true and re-confirm",
        ),
        PlanLookupError::Expired => RpcError::new(
            PLAN_NOT_FOUND,
            "plan_id has expired: register a fresh plan with dry_run=true and re-confirm",
        ),
        PlanLookupError::KindMismatch { expected, actual } => RpcError::new(
            PLAN_KIND_MISMATCH,
            format!("plan_id was registered for {actual}, not {expected}"),
        ),
        PlanLookupError::LibraryMismatch { expected, actual } => RpcError::new(
            PLAN_LIBRARY_MISMATCH,
            format!("plan_id was registered against library {actual:?}, not {expected:?}"),
        ),
    }
}

/// Build the wire envelope from a rendered [`Problem`]: the summary
/// line becomes `message`, the other three parts become `data`.
///
/// `message` stays self-sufficient on its own, so a client that reads
/// only that field learns no less than it did before `data` existed.
pub(crate) fn rpc_from_problem(code: i32, problem: Problem) -> RpcError {
    let mut err = RpcError::new(code, problem.summary);
    err.data = serde_json::to_value(problem.data).ok();
    err
}

fn from_ops(e: &OpsError) -> RpcError {
    use OpsError::*;
    let code = match e {
        IntakeNotFound { .. }
        | UnknownMetadataField { .. }
        | UnknownContributorRole { .. }
        | ContributorNotFound { .. }
        | NodeNotFound { .. }
        | NotALeaf { .. }
        | NotOrganizing { .. }
        | SourceNotArchived { .. } => INVALID_PARAMS,
        _ => INTERNAL_ERROR,
    };
    rpc_from_problem(code, e.explain())
}

fn from_ingest(e: &IngestError) -> RpcError {
    use IngestError::*;
    let code = match e {
        EmptyExtraction
        | NeedsOcr { .. }
        | UnknownIntake(_)
        | MissingEnvelope(_)
        | EnvelopeMismatch(_)
        | IntakeNotEmbedded(_)
        | OcrSourceStatusMismatch { .. }
        | OcrPagesMissing { .. }
        | OcrPagesExcess { .. } => INVALID_PARAMS,
        _ => INTERNAL_ERROR,
    };
    rpc_from_problem(code, e.explain())
}

fn from_glean(e: &GleanError) -> RpcError {
    use GleanError::*;
    let code = match e {
        NeedsOcr { .. }
        | UnknownIntake(_)
        | IntakeNotRebuildable(_)
        | MissingEnvelope(_)
        | EnvelopeMismatch(_) => INVALID_PARAMS,
        _ => INTERNAL_ERROR,
    };
    rpc_from_problem(code, e.explain())
}

fn from_registry(e: &RegistryError) -> RpcError {
    let code = match e {
        RegistryError::LibraryUnknown { .. } => INVALID_LIBRARY,
        RegistryError::Empty => INVALID_PARAMS,
        _ => INTERNAL_ERROR,
    };
    // No `Explain` impl on the registry errors yet, so the fallback
    // applies: a flattened summary and no hint.
    rpc_from_problem(code, Problem::from_error_chain(e))
}

/// Map a refusal a write command raised on its own input.
///
/// The `match` is exhaustive rather than defaulted: every variant is
/// caller input, so the only decision a new one carries is which
/// caller-input code it takes, and that decision should not have a
/// silent default.
fn from_cmd_input(e: &CmdInputError) -> RpcError {
    let code = match e {
        CmdInputError::UnknownIntake { .. }
        | CmdInputError::UnknownSha { .. }
        | CmdInputError::NotIngested { .. }
        | CmdInputError::BadArgument { .. }
        | CmdInputError::NothingToDo { .. }
        | CmdInputError::Refused { .. } => INVALID_PARAMS,
        CmdInputError::TargetDrifted { .. } => PLAN_TARGET_DRIFTED,
    };
    rpc_from_problem(code, e.explain())
}

fn from_config(e: &ConfigError) -> RpcError {
    let code = match e {
        ConfigError::UnknownLibrary { .. } => INVALID_LIBRARY,
        _ => INTERNAL_ERROR,
    };
    rpc_from_problem(code, Problem::from_error_chain(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eyre::WrapErr;

    #[test]
    fn ops_intake_not_found_is_invalid_params() {
        let err: Report = OpsError::IntakeNotFound { intake_id: 42 }.into();
        let rpc = write_err("metadata.set", err);
        assert_eq!(rpc.code, INVALID_PARAMS);
        assert!(rpc.message.contains("42"));
    }

    #[test]
    fn ops_unknown_field_is_invalid_params() {
        let err: Report = OpsError::UnknownMetadataField {
            field: "no_such_field".into(),
        }
        .into();
        let rpc = write_err("metadata.set", err);
        assert_eq!(rpc.code, INVALID_PARAMS);
        assert!(rpc.message.contains("no_such_field"));
    }

    #[test]
    fn ingest_unknown_intake_walks_context_chain() {
        let inner: Result<(), IngestError> = Err(IngestError::UnknownIntake(7));
        let err: Report = inner
            .context("rebuild step")
            .context("outer wrap")
            .unwrap_err();
        let rpc = write_err("corpus.rebuild", err);
        assert_eq!(rpc.code, INVALID_PARAMS);
    }

    #[test]
    fn glean_needs_ocr_is_invalid_params() {
        let err: Report = GleanError::NeedsOcr {
            reason: "no text layer".into(),
        }
        .into();
        let rpc = write_err("papers.corpus_rebuild", err);
        assert_eq!(rpc.code, INVALID_PARAMS);
        assert!(rpc.message.contains("no text layer"));
    }

    #[test]
    fn registry_library_unknown_is_invalid_library() {
        let err: Report = RegistryError::LibraryUnknown {
            name: "ghost".into(),
            available: vec!["main".into()],
        }
        .into();
        let rpc = write_err("library.set_default", err);
        assert_eq!(rpc.code, INVALID_LIBRARY);
    }

    #[test]
    fn config_unknown_library_is_invalid_library() {
        let rpc = config_err(ConfigError::UnknownLibrary {
            name: "ghost".into(),
            available: vec!["main".into()],
        });
        assert_eq!(rpc.code, INVALID_LIBRARY);
    }

    /// The transport reason must survive the boundary somewhere in the
    /// envelope. Before flattening, the wrapper's `Display` ("query
    /// error") was the whole message and the reason was simply gone.
    #[test]
    fn wrapper_error_keeps_its_root_cause_on_the_wire() {
        let err: Report = OpsError::Query(bookrack_query::QueryError::Embed(
            bookrack_embed::EmbedError::Unreachable("boom".into()),
        ))
        .into();
        let rpc = write_err("library.search", err);
        assert_eq!(rpc.code, INTERNAL_ERROR);
        let wire = serde_json::to_string(&rpc).expect("serialize");
        assert!(wire.contains("boom"), "root cause lost: {wire}");
    }

    #[test]
    fn rpc_error_carries_detail_and_hint_in_data() {
        let err: Report = OpsError::Query(bookrack_query::QueryError::Embed(
            bookrack_embed::EmbedError::ModelNotFound {
                model: "test-model".into(),
                reason: "model not found".into(),
            },
        ))
        .into();
        let rpc = write_err("library.search", err);
        let data: bookrack_core::ProblemData =
            serde_json::from_value(rpc.data.expect("data slot filled")).expect("ProblemData");
        assert!(
            data.detail.expect("detail").contains("404"),
            "the HTTP evidence belongs in detail"
        );
        assert!(data.hint.expect("hint").contains("ollama pull test-model"));
        assert!(!data.retryable);
    }

    /// A client that reads only `message` — every client written
    /// before `data` existed — must still learn what failed.
    #[test]
    fn rpc_message_alone_still_names_the_failure() {
        let explained: Report = OpsError::Query(bookrack_query::QueryError::Embed(
            bookrack_embed::EmbedError::ModelNotFound {
                model: "test-model".into(),
                reason: "model not found".into(),
            },
        ))
        .into();
        let rpc = write_err("library.search", explained);
        assert!(rpc.message.contains("test-model"), "{}", rpc.message);
        assert!(
            !rpc.message.contains("query error"),
            "a module name is not a failure: {}",
            rpc.message
        );

        // A variant with no wording of its own falls back to the
        // flattened chain, which is still self-sufficient.
        let unexplained: Report = IngestError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "source file is not readable",
        ))
        .into();
        let rpc = write_err("ingest.submit", unexplained);
        assert!(
            rpc.message.contains("source file is not readable"),
            "{}",
            rpc.message
        );
    }

    #[test]
    fn user_input_error_message_is_unchanged_by_flattening() {
        let e = OpsError::IntakeNotFound { intake_id: 42 };
        let expected = e.to_string(); // error-boundary-check: allow
        let rpc = from_ops(&e);
        assert_eq!(rpc.message, expected);
    }

    /// Every `CmdInputError` variant, with the code the caller sees.
    /// Written out rather than derived from `from_cmd_input`, which is
    /// the function under test.
    fn cmd_input_cases() -> Vec<(CmdInputError, i32)> {
        vec![
            (
                CmdInputError::UnknownIntake { intake_id: 999_999 },
                INVALID_PARAMS,
            ),
            (
                CmdInputError::UnknownSha {
                    sha: "deadbeef".into(),
                },
                INVALID_PARAMS,
            ),
            (
                CmdInputError::NotIngested {
                    what: "catalog",
                    hint: "Ingest a book into this library first.",
                },
                INVALID_PARAMS,
            ),
            (
                CmdInputError::BadArgument {
                    arg: "kind",
                    value: "nosuch".into(),
                    expected: "ivf-flat, hnsw".into(),
                },
                INVALID_PARAMS,
            ),
            (
                CmdInputError::NothingToDo {
                    summary: "no supported files found under \"/x\"".into(),
                    hint: "Point it at a directory holding a supported format.".into(),
                },
                INVALID_PARAMS,
            ),
            (
                CmdInputError::Refused {
                    summary: "library name is empty".into(),
                    hint: None,
                },
                INVALID_PARAMS,
            ),
            (
                CmdInputError::TargetDrifted {
                    intake_id: 7,
                    detail: "The intake was removed after the plan was minted.".into(),
                },
                PLAN_TARGET_DRIFTED,
            ),
        ]
    }

    #[test]
    fn every_cmd_input_variant_maps_onto_a_caller_input_code() {
        for (e, expected) in cmd_input_cases() {
            let label = format!("{e:?}");
            let rpc = write_err("remove", e.into());
            assert_eq!(rpc.code, expected, "{label}");
            assert_ne!(rpc.code, INTERNAL_ERROR, "{label}");
        }
    }

    /// The wording the variant wrote for itself must reach the wire
    /// intact — that is the whole reason the arm downcasts instead of
    /// letting the residual channel flatten the chain.
    #[test]
    fn cmd_input_hint_survives_onto_the_wire() {
        let err: Report = CmdInputError::UnknownIntake { intake_id: 999_999 }.into();
        let rpc = write_err("remove", err);
        assert!(rpc.message.contains("999999"), "{}", rpc.message);
        let data: bookrack_core::ProblemData =
            serde_json::from_value(rpc.data.expect("data slot filled")).expect("ProblemData");
        assert!(data.hint.expect("hint").contains("bookrack list"));
        assert!(!data.retryable);
    }

    /// A write command folds its refusal through `?` and `.context()`
    /// on the way up, so the arm has to find the type below the wraps.
    /// This also pins the premise that the arm needs no help from the
    /// ops-error arms above it: nothing on this path is an `OpsError`.
    #[test]
    fn cmd_input_error_walks_context_chain() {
        let inner: Result<(), CmdInputError> = Err(CmdInputError::TargetDrifted {
            intake_id: 7,
            detail: "The intake was removed after the plan was minted.".into(),
        });
        let err: Report = inner
            .context("execute remove plan")
            .context("remove")
            .unwrap_err();
        let rpc = write_err("remove", err);
        assert_eq!(rpc.code, PLAN_TARGET_DRIFTED);
        assert!(rpc.message.contains("book 7"), "{}", rpc.message);
    }

    #[test]
    fn unknown_error_falls_through_to_internal() {
        let err: Report = eyre::eyre!("disk on fire");
        let rpc = write_err("vectors.rebuild", err);
        assert_eq!(rpc.code, INTERNAL_ERROR);
        assert!(rpc.message.contains("vectors.rebuild"));
        assert!(rpc.message.contains("disk on fire"));
    }
}
