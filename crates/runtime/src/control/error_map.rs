// SPDX-License-Identifier: Apache-2.0

//! Put a control-plane handler's error onto the right JSON-RPC code.
//!
//! Write-class RPCs receive `eyre::Report` from the `cmd::*` layer
//! (which folds typed downstream errors through `?`/`.context()`); the
//! `library.*` read proxies receive an [`OpsError`] straight from
//! `bookrack_ops::reads`. Reporting every such error as
//! [`INTERNAL_ERROR`] hides user-input failures — unknown intakes,
//! validation refusals, unknown libraries — from MCP/CLI clients,
//! who then cannot distinguish a caller-side input problem from a
//! genuine server fault. Both classes route through this one module,
//! so the same condition takes the same code whichever side raised it.
//!
//! The judgement is [`bookrack_ops::error_class`]'s: it reads the
//! pipeline wrappers and the store and backend leaves, and returns an
//! [`ErrorKind`] with the failure's three-part [`Problem`]. This module
//! maps each kind onto its code, and adds what only the host knows: the
//! refusals a write command raises on its own, before it reaches the
//! layers below ([`CmdInputError`], [`CmdStateError`]; a bare `bail!`
//! is not classified), a refused change to the mounted set, a plan
//! lookup that failed, and the residual channel.
//!
//! [`handler_err`] walks the `eyre` cause chain outermost first and
//! asks of each cause the ops tier, the host's command types, and the
//! leaf tier; the first cause one of them reads decides. Anything that
//! matches no known type falls through to [`INTERNAL_ERROR`].
//!
//! Two of those fall-throughs are exits nothing has judged: the chain
//! that matched no known type at all, and an `OpsError::Other` whose
//! report the classifier cannot see into. Each leaves a `warn` event on
//! the daemon side — `unclassified handler error` with the method and
//! the flattened chain from here, `unclassified ops error` with the
//! chain from the classifier — so a failure the reply alone would
//! otherwise record reaches the daemon log. A variant judged a fault is
//! a verdict, and is not logged.
//!
//! [`rpc_from_problem`] splits a [`Problem`] across the envelope: the
//! summary into `message`, the detail / hint / retryable triple into
//! `data`. A type with no `Explain` impl takes
//! [`Problem::from_error_chain`], whose summary is the flattened cause
//! chain — a bare `to_string()` here would drop the root cause at the
//! process boundary. `scripts/error-boundary-check.sh` enforces that.

use std::ops::ControlFlow;

use bookrack_config::ConfigError;
use bookrack_core::{Explain, Problem, ProblemData};
use bookrack_ops::OpsError;
use bookrack_ops::dto::UnknownFilterValue;
use bookrack_ops::error_class::{self, ClassifiedError, ErrorKind};
use bookrack_ops::methods::dispatch::DispatchError;
use bookrack_ops::reads::reference::ReferenceError;
use bookrack_ops::registry::RegistryError;
use eyre::Report;

use super::jsonrpc::{
    BACKEND_UNAVAILABLE, BUSY, INTERNAL_ERROR, INVALID_LIBRARY, INVALID_PARAMS, PLAN_KIND_MISMATCH,
    PLAN_LIBRARY_MISMATCH, PLAN_NOT_FOUND, PLAN_TARGET_DRIFTED, RpcError, STATE_UNUSABLE,
};
use super::methods::session::SessionError;
use super::plan_registry::PlanLookupError;
use crate::cmd::input_error::CmdInputError;
use crate::cmd::state_error::CmdStateError;
use crate::mount::MountRefusal;
use crate::unmounted::{MountOccasion, Repair};

/// Map a handler's `eyre::Report` onto a JSON-RPC error envelope.
///
/// `method` is the wire-name of the failing RPC (`"metadata.set"`,
/// `"corpus.rebuild"`, ...), used only to label the residual
/// [`INTERNAL_ERROR`] message.
///
/// The MCP front end re-wraps the returned envelope in its own error
/// type without re-deciding the code, so a handler error shared by
/// both surfaces takes one code on each.
///
/// An error the walk recognises no type in still leaves through
/// [`rpc_from_problem`], so `data` is on the envelope unconditionally:
/// `retryable` alone, since such an error has neither evidence nor a
/// next step to name. It is also the one exit no classifier has
/// judged, so it leaves a `warn` event on the daemon side carrying the
/// method and the flattened chain; the reply alone would otherwise be
/// the only record of it.
pub fn handler_err(method: &str, err: Report) -> RpcError {
    classify(&err).unwrap_or_else(|| {
        let chain = format!("{err:#}");
        tracing::warn!(method, error = %chain, "unclassified handler error");
        rpc_from_problem(
            INTERNAL_ERROR,
            Problem::new(format!("{method} failed: {chain}")),
        )
    })
}

/// The three-part reading of `err` for a surface that is not an RPC
/// reply: the classified wording when the chain carries a type this
/// map knows — the inverse of [`rpc_from_problem`], so the two
/// surfaces word one failure alike — and the flattened chain otherwise.
pub(crate) fn report_problem(err: &Report) -> Problem {
    match classify(err) {
        Some(rpc) => problem_of(rpc),
        None => Problem::from_error_chain(err.as_ref()),
    }
}

/// The reason a library that failed to open with `err` is not served,
/// and the kind of repair its hint names.
///
/// `library` is the name the registry carries the library under, or
/// `None` for a root selected by path. A catalog holding retired labels
/// is worded with that name in its hint, and the hint replaces the
/// library; every other error reads as [`report_problem`] reads it and
/// is repaired in place.
pub(crate) fn skip_reason(library: Option<&str>, err: &Report) -> (Problem, Repair) {
    match error_class::retired_labels_reason(library, err) {
        Some(problem) => (problem, Repair::Replacement),
        None => (report_problem(err), Repair::InPlace),
    }
}

/// The three-part reading of `err` when a store it depends on is in a
/// state this build cannot serve, and `None` for every other error.
///
/// For a command that opens a store itself, with no daemon in between:
/// such a refusal names the store, the versions, and the operator's
/// next step, and reads the same as the daemon's reply for it. Nothing
/// else is let through, so an error with no next step keeps the cause
/// chain its caller attached.
pub fn state_refusal(err: &Report) -> Option<Problem> {
    classify(err)
        .filter(|rpc| rpc.code == STATE_UNUSABLE)
        .map(problem_of)
}

/// The three-part reading of `err` when it is a refusal of the
/// caller's input, and `None` for every other error.
///
/// For a command that checks input itself, with no daemon in between:
/// the refusal reads the same as the daemon's `-32602` reply for it.
/// Nothing else is let through, so an error with no next step keeps the
/// cause chain its caller attached.
pub fn input_refusal(err: &Report) -> Option<Problem> {
    classify(err)
        .filter(|rpc| rpc.code == INVALID_PARAMS)
        .map(problem_of)
}

/// The problem an envelope was built from: the inverse of
/// [`rpc_from_problem`].
fn problem_of(rpc: RpcError) -> Problem {
    let data = rpc
        .data
        .and_then(|data| serde_json::from_value::<bookrack_core::ProblemData>(data).ok())
        .unwrap_or(bookrack_core::ProblemData {
            detail: None,
            hint: None,
            retryable: false,
        });
    Problem {
        summary: rpc.message,
        data,
    }
}

/// The envelope `err` takes when its chain carries a type this map
/// knows, or `None` when it does not and the caller falls back to the
/// residual `INTERNAL_ERROR`.
///
/// Each cause is asked of the ops tier, the host's command types, and
/// the leaf tier in turn; the first cause one of them reads decides. A
/// leaf that reads its cause and holds no judgement ends the walk
/// unclassified.
fn classify(err: &Report) -> Option<RpcError> {
    for cause in err.chain() {
        if let Some(classified) = error_class::classify_ops_cause(cause) {
            return Some(rpc_from_classified(classified));
        }
        if let Some(e) = cause.downcast_ref::<CmdInputError>() {
            return Some(from_cmd_input(e));
        }
        if let Some(e) = cause.downcast_ref::<CmdStateError>() {
            return Some(from_cmd_state(e));
        }
        if let ControlFlow::Break(found) = error_class::classify_leaf_cause(cause) {
            return found.map(rpc_from_classified);
        }
    }
    None
}

/// The diagnostic a failed queue job records: the summary and data of
/// the envelope [`handler_err`] would build for the same error, read
/// back as one [`Problem`]. `None` when no type on the chain is known;
/// the job then keeps only its flattened text.
pub fn job_failure_problem(err: &Report) -> Option<Problem> {
    let rpc = classify(err)?;
    let data = match rpc.data {
        Some(data) => match serde_json::from_value::<ProblemData>(data) {
            Ok(data) => data,
            Err(e) => {
                // `rpc_from_problem` wrote this value from a `ProblemData`;
                // failing to read it back is a contract break worth a
                // line in the log, not a reason to drop the summary.
                tracing::warn!(error = %e, "queue: envelope data is not a ProblemData");
                ProblemData {
                    detail: None,
                    hint: None,
                    retryable: false,
                }
            }
        },
        None => ProblemData {
            detail: None,
            hint: None,
            retryable: false,
        },
    };
    Some(Problem {
        summary: rpc.message,
        data,
    })
}

/// Map a directly-held [`OpsError`] without an `eyre` round-trip: the
/// read proxies and the MCP tools get the typed error straight from
/// `bookrack_ops` and classify it here, with the same judgement the
/// write side reaches through [`handler_err`]. The MCP front end
/// re-wraps the returned envelope in its own error type without
/// re-deciding the code, so one typed error takes one code on both
/// surfaces.
pub fn ops_err(e: OpsError) -> RpcError {
    rpc_from_classified(error_class::from_ops(&e))
}

/// Map a reference read or write failure onto its wire code. The
/// control plane's `reference.lookup` and the MCP `reference.lookup`
/// and `reference.overlay_set` tools all come here, so one failure
/// takes one code on every surface: caller input (`-32602`), a store at
/// a schema this build does not read (`-32018`), or an internal error
/// carrying its full cause chain.
pub fn reference_err(e: ReferenceError) -> RpcError {
    rpc_from_classified(error_class::from_reference(&e))
}

/// Map a failure of a method from the ops method table. A failure the
/// dispatcher judged takes its kind's code; a failure of the host's own
/// hook is judged here, as any handler's error is.
pub fn dispatch_err(e: DispatchError) -> RpcError {
    match e {
        DispatchError::Judged(classified) => rpc_from_classified(classified),
        DispatchError::Host { method, report } => handler_err(method, report),
    }
}

/// Map a session tool's failure: params that do not fit and an unknown
/// state filter are caller input; a registry that could not be read is
/// judged as the registry judges it; a poisoned queue lock, a reply
/// that did not serialize, and a tool with no body are faults.
pub fn session_err(e: SessionError) -> RpcError {
    match &e {
        SessionError::Registry(registry) => {
            rpc_from_classified(error_class::from_registry(registry))
        }
        SessionError::UnknownState(unknown) => unknown_filter_value(unknown),
        SessionError::Params(params) => rpc_from_classified(error_class::from_params(params)),
        SessionError::QueueStateLock
        | SessionError::Encode { .. }
        | SessionError::NoBody { .. } => rpc_from_classified(error_class::from_server_fault(&e)),
    }
}

/// Map a directly-held [`RegistryError`] without an `anyhow` round-trip.
pub(crate) fn registry_err(e: RegistryError) -> RpcError {
    rpc_from_classified(error_class::from_registry(&e))
}

/// Map a directly-held [`ConfigError`] from a registry write onto the
/// corresponding wire code. An unknown library named against the
/// on-disk registry is caller input ([`INVALID_LIBRARY`]); every other
/// registry fault is a server-side [`INTERNAL_ERROR`].
pub(crate) fn config_err(e: ConfigError) -> RpcError {
    rpc_from_classified(error_class::from_config(&e))
}

/// Map a refused runtime mount of `name` onto the corresponding wire
/// code.
///
/// Every reason a mount refuses with names the repair and no retry, as
/// the reason bring-up skips a library for does; this closes each hint
/// with the mount command, as bring-up closes the record it keeps, so a
/// mount after a skip reads the skip's hint word for word. A reason
/// without a hint — an unclassified fault, a name the registry does not
/// carry — names no repair for the mount to follow and stays as it is.
///
/// A reason whose hint replaces the library — a catalog holding retired
/// labels — names `name` in that hint and is followed by no mount.
pub(crate) fn mount_err(name: &str, e: MountRefusal) -> RpcError {
    refusal_rpc(e, Some(name), |problem, repair| {
        MountOccasion::Serving.close(name, problem, repair)
    })
}

/// Map a refused unmount onto the corresponding wire code, each reason
/// as it is: none of them is repaired by a mount.
pub(crate) fn unmount_err(e: MountRefusal) -> RpcError {
    refusal_rpc(e, None, |problem, _| problem)
}

/// The envelope a change to the mounted set is refused with, `close`
/// applied to every reason the mounter worded. `library` is the
/// registry name the change is for, when the caller holds one for a
/// reason to name.
///
/// The `match` is exhaustive: each variant already knows whether it is
/// caller input, a refusal the caller can repair, or a fault, so the
/// only decision a new one carries is which code it takes — and that
/// decision should not have a silent default. The refusals carry their
/// own three-part [`Problem`]; an entry that does not resolve and a
/// library that fails to open are worded as bring-up words the same
/// failure when it skips the library. The registry's own refusals —
/// already mounted, not mounted — are worded by the registry and left
/// as they are.
fn refusal_rpc(
    e: MountRefusal,
    library: Option<&str>,
    close: impl Fn(Problem, Repair) -> Problem,
) -> RpcError {
    match e {
        MountRefusal::Unresolvable { library, source } => {
            let code = match source.as_ref() {
                ConfigError::UnknownLibrary { .. } => INVALID_LIBRARY,
                // The root is not there: the next step is the
                // operator's, and the same call resent will not help.
                ConfigError::DataDirNotFound(_) => STATE_UNUSABLE,
                _ => INTERNAL_ERROR,
            };
            rpc_from_problem(
                code,
                close(
                    crate::unmounted::resolve_problem(&library, &source),
                    Repair::InPlace,
                ),
            )
        }
        MountRefusal::Registry(e) => registry_err(e),
        MountRefusal::Refused { problem, .. } => {
            rpc_from_problem(INVALID_PARAMS, close(problem, Repair::InPlace))
        }
        MountRefusal::RootLocked { problem, .. } => {
            rpc_from_problem(BACKEND_UNAVAILABLE, close(problem, Repair::InPlace))
        }
        MountRefusal::BringUp(err) => {
            let code = classify(&err).map_or(INTERNAL_ERROR, |rpc| rpc.code);
            let (problem, repair) = skip_reason(library, &err);
            rpc_from_problem(code, close(problem, repair))
        }
    }
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

/// Map a refused filter value to `INVALID_PARAMS`.
///
/// The accepted set travels with the refusal rather than being spelled
/// out here, so a vocabulary that gains a value cannot leave a stale
/// list behind on this surface.
pub(crate) fn unknown_filter_value(unknown: &UnknownFilterValue) -> RpcError {
    rpc_from_classified(error_class::from_unknown_filter(unknown))
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

/// Build the wire envelope from a classified failure: its kind picks
/// the code, its problem fills `message` and `data`.
fn rpc_from_classified(classified: ClassifiedError) -> RpcError {
    rpc_from_problem(code_of(classified.kind), classified.problem)
}

/// The code each kind of failure takes on the control socket.
fn code_of(kind: ErrorKind) -> i32 {
    match kind {
        ErrorKind::InvalidParams => INVALID_PARAMS,
        ErrorKind::Internal => INTERNAL_ERROR,
        ErrorKind::Busy => BUSY,
        ErrorKind::InvalidLibrary => INVALID_LIBRARY,
        ErrorKind::BackendUnavailable => BACKEND_UNAVAILABLE,
        ErrorKind::StateUnusable => STATE_UNUSABLE,
    }
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

/// Map a library state a write command refused to act on. Every
/// variant is the operator's to resolve, so each takes
/// [`STATE_UNUSABLE`]; the `match` is exhaustive so a new variant is
/// read against that claim before it ships.
fn from_cmd_state(e: &CmdStateError) -> RpcError {
    let code = match e {
        CmdStateError::VectorTableMissing { .. } => STATE_UNUSABLE,
    };
    rpc_from_problem(code, e.explain())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_ops::pipeline::book::IngestError;
    use bookrack_refs::RefsError;
    use eyre::WrapErr;

    /// The code each kind takes is the control plane's documented
    /// contract, written out here rather than derived from `code_of`,
    /// which is what is under test.
    #[test]
    fn every_error_kind_takes_its_documented_code() {
        for (kind, code) in [
            (ErrorKind::InvalidParams, -32602),
            (ErrorKind::Internal, -32603),
            (ErrorKind::Busy, -32001),
            (ErrorKind::InvalidLibrary, -32010),
            (ErrorKind::BackendUnavailable, -32017),
            (ErrorKind::StateUnusable, -32018),
        ] {
            assert_eq!(code_of(kind), code, "{kind:?}");
        }
    }

    /// Each entry point hands its error to the classifier and puts the
    /// judgement on the envelope: one failure per entry, with the code
    /// and the wording the classifier gave it.
    #[test]
    fn every_entry_point_puts_the_classifiers_judgement_on_the_envelope() {
        let busy = ops_err(OpsError::WriteBusy(bookrack_ops::write_session::WriteBusy));
        assert_eq!(busy.code, BUSY);
        assert_eq!(busy.message, "another write command is already in progress");
        let data = busy.data.expect("problem data");
        assert_eq!(data["retryable"], serde_json::json!(true), "{data}");

        let unknown = registry_err(RegistryError::LibraryUnknown {
            name: "ghost".into(),
            available: vec!["alpha".into()],
        });
        assert_eq!(unknown.code, INVALID_LIBRARY);
        assert_eq!(unknown.message, "no library named \"ghost\"");

        let config = config_err(ConfigError::UnknownLibrary {
            name: "ghost".into(),
            available: vec!["main".into()],
        });
        assert_eq!(config.code, INVALID_LIBRARY);

        let reference = reference_err(ReferenceError::Open {
            path: std::path::PathBuf::from("/some/where/reference.db"),
            source: RefsError::SchemaTooOld {
                found: 1,
                supported: 2,
            },
        });
        assert_eq!(reference.code, STATE_UNUSABLE, "{}", reference.message);
        assert_eq!(reference.message, "cannot read the reference store");

        let filter = unknown_filter_value(&UnknownFilterValue {
            parameter: "status",
            value: "nosuch".into(),
            accepted: vec!["done", "failed"],
        });
        assert_eq!(filter.code, INVALID_PARAMS);
        assert_eq!(filter.message, "cannot filter on status value \"nosuch\"");

        let unreachable = handler_err(
            "stamps.reconcile",
            bookrack_embed::EmbedError::Unreachable("connection refused".into()).into(),
        );
        assert_eq!(unreachable.code, BACKEND_UNAVAILABLE);
    }

    /// A reranker that did not answer is the backend code on the control
    /// socket, retryable, whether a read proxy holds the typed error or
    /// a handler folds it into its report — the code the CLI turns into
    /// exit 4.
    #[test]
    fn a_reranker_that_did_not_answer_takes_the_backend_code() {
        use bookrack_rerank::RerankError;
        let direct = ops_err(OpsError::Rerank(RerankError::Unreachable(
            "connection refused".into(),
        )));
        let folded = handler_err(
            "library.search",
            Report::from(OpsError::Rerank(RerankError::Overloaded {
                status: 503,
                body: "loading model".into(),
            }))
            .wrap_err("rerank the candidates"),
        );
        for (label, rpc) in [("direct", direct), ("folded", folded)] {
            assert_eq!(rpc.code, BACKEND_UNAVAILABLE, "{label}: {}", rpc.message);
            let data: bookrack_core::ProblemData =
                serde_json::from_value(rpc.data.expect("data slot filled")).expect("ProblemData");
            assert!(data.retryable, "{label}");
        }
    }

    /// The outermost cause the walk recognises decides, whichever tier
    /// it belongs to. An untyped ops report is judged by the wrapper it
    /// arrived in, so a command refusal or a store leaf it holds — both
    /// of which the walk reads when they arrive bare — does not reach
    /// the envelope.
    #[test]
    fn the_outermost_recognised_cause_decides_across_tiers() {
        let over_command: Report = OpsError::Other(
            Report::from(CmdInputError::UnknownIntake { intake_id: 7 }).wrap_err("remove step"),
        )
        .into();
        assert!(
            over_command
                .chain()
                .any(|cause| cause.downcast_ref::<CmdInputError>().is_some()),
            "the command refusal is on the chain"
        );
        let over_leaf: Report = OpsError::Other(
            Report::from(bookrack_catalog::CatalogError::SchemaTooNew {
                found: 99,
                expected: 17,
            })
            .wrap_err("open catalog"),
        )
        .into();
        assert!(
            over_leaf.chain().any(|cause| cause
                .downcast_ref::<bookrack_catalog::CatalogError>()
                .is_some()),
            "the store leaf is on the chain"
        );
        for (label, err) in [
            ("over a command refusal", over_command),
            ("over a store leaf", over_leaf),
        ] {
            let rpc = handler_err("remove", err);
            assert_eq!(rpc.code, INTERNAL_ERROR, "{label}: {}", rpc.message);
            assert!(
                !rpc.message.starts_with("remove failed:"),
                "{label}: the ops wrapper judged it, not the residual channel: {}",
                rpc.message
            );
        }
    }

    /// `report_problem` is `rpc_from_problem`'s inverse on a classified
    /// chain, and the flattened chain on one the map does not know.
    #[test]
    fn report_problem_round_trips_a_classified_chain() {
        let err: Report = RegistryError::LibraryUnknown {
            name: "ghost".into(),
            available: vec!["alpha".into()],
        }
        .into();
        let direct = RegistryError::LibraryUnknown {
            name: "ghost".into(),
            available: vec!["alpha".into()],
        }
        .explain();
        let err = err.wrap_err("bring up library 'ghost'");
        assert_eq!(report_problem(&err), direct);

        let unknown: Report = eyre::eyre!("a cause this map does not know");
        let unknown = unknown.wrap_err("resolve configuration for library 'beta'");
        let problem = report_problem(&unknown);
        assert!(
            problem.summary.contains("resolve configuration"),
            "{}",
            problem.summary
        );
        assert!(
            problem.summary.contains("does not know"),
            "{}",
            problem.summary
        );
        assert!(problem.data.hint.is_none());
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
        let rpc = handler_err("library.search", err);
        assert_eq!(rpc.code, BACKEND_UNAVAILABLE);
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
        let rpc = handler_err("library.search", err);
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
        let rpc = handler_err("library.search", explained);
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
        let rpc = handler_err("ingest.submit", unexplained);
        assert!(
            rpc.message.contains("source file is not readable"),
            "{}",
            rpc.message
        );
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
            let rpc = handler_err("remove", e.into());
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
        let rpc = handler_err("remove", err);
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
        let rpc = handler_err("remove", err);
        assert_eq!(rpc.code, PLAN_TARGET_DRIFTED);
        assert!(rpc.message.contains("book 7"), "{}", rpc.message);
    }

    /// A library state a write command refuses to act on is the
    /// operator's to resolve: the state code, with the step that
    /// resolves it.
    #[test]
    fn a_command_state_refusal_takes_the_state_code_with_a_next_step() {
        let err: Report = CmdStateError::VectorTableMissing {
            dim: 1024,
            reset_command: "bookrack vectors reset",
        }
        .into();
        let rpc = handler_err("vectors.rebuild", err);
        assert_eq!(rpc.code, STATE_UNUSABLE, "{}", rpc.message);
        let data: bookrack_core::ProblemData =
            serde_json::from_value(rpc.data.expect("data slot filled")).expect("ProblemData");
        assert!(
            !data.hint.unwrap_or_default().trim().is_empty(),
            "a state code without the operator's next step"
        );
        assert!(!data.retryable);
    }

    fn retired_catalog() -> bookrack_catalog::CatalogError {
        bookrack_catalog::CatalogError::RetiredLabels {
            path: std::path::PathBuf::from("/disk/library/beta/catalog.db"),
            hits: vec![
                bookrack_dbkit::RetiredHit {
                    marker: "format = ocr-markdown",
                    count: 3,
                },
                bookrack_dbkit::RetiredHit {
                    marker: "derived_from_sha256",
                    count: 2,
                },
            ],
        }
    }

    /// The reason a library is skipped for names the library only when
    /// the caller holds its registry name, and says whether the hint
    /// leaves a library to retry against.
    #[test]
    fn a_skip_reason_names_a_registered_library_and_marks_a_replacement() {
        let err = Report::from(retired_catalog()).wrap_err("bring up library 'default'");
        let (named, repair) = skip_reason(Some("beta"), &err);
        assert_eq!(repair, Repair::Replacement);
        let hint = named.data.hint.unwrap_or_default();
        assert!(
            hint.contains("`bookrack libraries remove beta --purge`"),
            "{hint}"
        );

        let (unnamed, repair) = skip_reason(None, &err);
        assert_eq!(repair, Repair::Replacement);
        let hint = unnamed.data.hint.unwrap_or_default();
        assert!(!hint.contains("default"), "{hint}");
        assert!(hint.contains("'/disk/library/beta'"), "{hint}");

        let behind = Report::from(bookrack_catalog::CatalogError::SchemaTooNew {
            found: 19,
            expected: 18,
        });
        let (reason, repair) = skip_reason(Some("beta"), &behind);
        assert_eq!(repair, Repair::InPlace);
        assert_eq!(reason, report_problem(&behind));
    }

    /// A reference store fault with no next step is not classified: it
    /// keeps the residual code and the context its caller attached.
    #[test]
    fn a_reference_store_fault_with_no_next_step_keeps_its_context() {
        let err = Report::from(RefsError::Io(std::io::Error::other(
            "backup directory is not writable",
        )))
        .wrap_err("preflight reference store schema check failed");
        assert!(state_refusal(&err).is_none());
        let rpc = handler_err("library.mount", err);
        assert_eq!(rpc.code, INTERNAL_ERROR, "{}", rpc.message);
        assert!(
            rpc.message
                .contains("preflight reference store schema check failed")
                && rpc.message.contains("backup directory is not writable"),
            "{}",
            rpc.message
        );
    }

    /// A reference store behind this build, met by a command that opens
    /// it itself, reads as the daemon's reply for it would.
    #[test]
    fn state_refusal_passes_a_reference_store_behind_this_build() {
        let err = Report::from(RefsError::SchemaTooOld {
            found: 1,
            supported: 2,
        })
        .wrap_err("open /some/where/reference.db");
        let problem = state_refusal(&err).expect("a state refusal");
        assert!(
            problem
                .summary
                .starts_with("reference.db schema is behind this build"),
            "{}",
            problem.summary
        );
        let hint = problem.data.hint.unwrap_or_default();
        assert!(
            hint.contains("bookrack run") && hint.contains("bookrack distill build"),
            "the hint names both steps that migrate it: {hint}"
        );
    }

    /// The exit for a command that opens a store itself lets a state
    /// refusal through with its three parts and nothing else.
    #[test]
    fn state_refusal_passes_state_refusals_and_nothing_else() {
        use bookrack_catalog::CatalogError;
        let refused: Report = Report::from(CatalogError::SchemaTooNew {
            found: 99,
            expected: 17,
        })
        .wrap_err("open /some/where/catalog.db");
        let problem = state_refusal(&refused).expect("a state refusal");
        assert!(
            problem.summary.starts_with("catalog schema is newer"),
            "{}",
            problem.summary
        );
        assert!(problem.data.hint.is_some());

        let caller_input: Report = OpsError::IntakeNotFound { intake_id: 7 }.into();
        assert!(
            handler_err("m", OpsError::IntakeNotFound { intake_id: 7 }.into()).code
                != STATE_UNUSABLE
        );
        assert!(state_refusal(&caller_input).is_none());

        let unclassified = eyre::eyre!("disk on fire");
        assert!(state_refusal(&unclassified).is_none());
    }

    #[test]
    fn unknown_error_falls_through_to_internal() {
        let err: Report = eyre::eyre!("disk on fire");
        let rpc = handler_err("vectors.rebuild", err);
        assert_eq!(rpc.code, INTERNAL_ERROR);
        assert!(rpc.message.contains("vectors.rebuild"));
        assert!(rpc.message.contains("disk on fire"));
    }

    /// `data` is on the envelope unconditionally: `docs/control-plane.md`
    /// promises that a type which has written no wording of its own still
    /// sends `data` with `retryable` alone, and the residual channel is
    /// where those errors end up. The message is asserted here as well,
    /// because filling the slot must not cost the method label or the
    /// root cause that make the residual message self-sufficient.
    #[test]
    fn the_residual_channel_fills_the_data_slot() {
        let err: Report = eyre::eyre!("disk on fire");
        let rpc = handler_err("vectors.rebuild", err);
        assert_eq!(rpc.code, INTERNAL_ERROR);
        assert!(rpc.message.contains("vectors.rebuild failed:"));
        assert!(rpc.message.contains("disk on fire"));
        let data: bookrack_core::ProblemData =
            serde_json::from_value(rpc.data.expect("data slot filled")).expect("ProblemData");
        assert!(!data.retryable);
        assert!(
            data.detail.is_none() && data.hint.is_none(),
            "an unclassified error has no evidence and no next step to offer"
        );
    }

    /// Run `f` under a subscriber that captures every event it emits,
    /// and return the captured events.
    fn captured(f: impl FnOnce()) -> Vec<bookrack_obs::LogEvent> {
        use tracing_subscriber::layer::SubscriberExt;
        let handle = bookrack_obs::LogStreamHandle::new(16, 16);
        let subscriber = tracing_subscriber::Registry::default()
            .with(bookrack_obs::BroadcastLayer::new(handle.clone()));
        tracing::subscriber::with_default(subscriber, f);
        handle.tail(16)
    }

    /// An error the walk recognises no type in is a `-32603` nobody has
    /// judged, so it leaves a record on the daemon side: one warning
    /// naming the method and the flattened chain. The envelope is the
    /// same as before.
    #[test]
    fn the_residual_tail_warns_on_the_daemon_side() {
        let mut rpc = None;
        let events = captured(|| {
            let err: Report = eyre::eyre!("disk on fire").wrap_err("rebuild step");
            rpc = Some(handler_err("vectors.rebuild", err));
        });
        let rpc = rpc.expect("handler_err ran");
        assert_eq!(rpc.code, INTERNAL_ERROR);
        assert_eq!(
            rpc.message,
            "vectors.rebuild failed: rebuild step: disk on fire"
        );
        let [event] = events.as_slice() else {
            panic!("one warning, got {events:?}");
        };
        assert_eq!(event.level, "WARN");
        assert_eq!(event.message, "unclassified handler error");
        assert_eq!(
            event.fields.get("method").and_then(|v| v.as_str()),
            Some("vectors.rebuild")
        );
        let chain = event
            .fields
            .get("error")
            .and_then(|v| v.as_str())
            .expect("error field");
        assert!(
            chain.contains("rebuild step") && chain.contains("disk on fire"),
            "{chain}"
        );
    }

    /// `OpsError::Other` is the one shape the recursion cannot see
    /// into, so what it holds has not been judged either; it leaves
    /// the same kind of record, on the read path the MCP server also
    /// takes.
    #[test]
    fn an_untyped_ops_report_warns_on_the_daemon_side() {
        let mut rpc = None;
        let events = captured(|| {
            rpc = Some(ops_err(OpsError::Other(eyre::eyre!("boom"))));
        });
        let rpc = rpc.expect("ops_err ran");
        assert_eq!(rpc.code, INTERNAL_ERROR);
        let [event] = events.as_slice() else {
            panic!("one warning, got {events:?}");
        };
        assert_eq!(event.level, "WARN");
        assert_eq!(event.message, "unclassified ops error");
        let chain = event
            .fields
            .get("error")
            .and_then(|v| v.as_str())
            .expect("error field");
        assert!(chain.contains("boom"), "{chain}");
    }

    /// A variant a `from_*` judged — as caller input or as a fault — is
    /// a verdict, not a fall-through, and leaves no record: the warning
    /// counts only the errors nothing has judged.
    #[test]
    fn a_judged_error_leaves_no_warning() {
        let judged: [(&str, Report); 3] = [
            (
                "a fault",
                bookrack_embed::EmbedError::UnusableVector {
                    index: 1,
                    defect: bookrack_core::VectorDefect::NonFinite,
                }
                .into(),
            ),
            (
                "caller input",
                OpsError::IntakeNotFound { intake_id: 42 }.into(),
            ),
            (
                "a wrapped fault",
                OpsError::Query(bookrack_query::QueryError::Embed(
                    bookrack_embed::EmbedError::MalformedResponse("no vectors".into()),
                ))
                .into(),
            ),
        ];
        for (label, err) in judged {
            let events = captured(|| {
                handler_err("vectors.rebuild", err);
            });
            assert!(events.is_empty(), "{label}: {events:?}");
        }
    }
}
