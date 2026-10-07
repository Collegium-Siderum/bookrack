// SPDX-License-Identifier: Apache-2.0

//! Classify a control-plane handler's error onto the right JSON-RPC
//! code.
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
//! [`handler_err`] walks the `eyre` cause chain looking for known typed
//! errors and, when one matches, maps a user-input variant onto
//! [`INVALID_PARAMS`] (or the bookrack-specific code reserved for that
//! shape, e.g. [`INVALID_LIBRARY`]). Anything that does not match a
//! known user-input variant falls through to [`INTERNAL_ERROR`].
//!
//! The walk recognises two tiers of type. The pipeline wrappers —
//! [`OpsError`], [`IngestError`], [`GleanError`], [`QueryError`] — are
//! raised by the layers below `cmd::*` and carry their own caller-input
//! variants; a refusal a write command makes on its own, before it
//! reaches those layers, is only classified if the command raises
//! [`CmdInputError`] rather than a bare `bail!`. The leaves —
//! [`EmbedError`], [`VectorsError`], [`CatalogError`], [`CorpusError`],
//! [`RegistryError`] — are the store and backend types the wrappers
//! fold in.
//!
//! Classification is recursive: a wrapper's `from_*` delegates every
//! variant that holds a leaf to that leaf's own `from_*`, so a leaf
//! reaches the same code whether a command raised it bare or a wrapper
//! (or two — `OpsError::Query(QueryError::Vectors(..))`) carried it.
//! The one shape the recursion cannot see through is
//! `OpsError::Other(eyre::Report)`: the report inside is opaque to the
//! `match`, and whatever typed error it holds takes the wrapper's
//! residual code.
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
//!
//! The exception to "wording is not written here" is the hint on a
//! leaf the classifier judges caller input. The leaf store types write
//! no wording of their own, and a caller-input code without a next
//! step is an empty promise, so the `from_*` for those types attaches
//! the step beside the code it chose. That is the classifier stating
//! what its own judgement implies, not the leaf inventing facts about
//! itself: the summary stays the leaf's flattened chain.

use bookrack_catalog::CatalogError;
use bookrack_config::ConfigError;
use bookrack_core::{Explain, Problem};
use bookrack_corpus::CorpusError;
use bookrack_embed::EmbedError;
use bookrack_glean::GleanError;
use bookrack_ingest::IngestError;
use bookrack_ops::OpsError;
use bookrack_ops::dto::UnknownFilterValue;
use bookrack_ops::registry::RegistryError;
use bookrack_query::QueryError;
use bookrack_vectors::VectorsError;
use eyre::Report;

use super::jsonrpc::{
    BACKEND_UNAVAILABLE, INTERNAL_ERROR, INVALID_LIBRARY, INVALID_PARAMS, PLAN_KIND_MISMATCH,
    PLAN_LIBRARY_MISMATCH, PLAN_NOT_FOUND, PLAN_TARGET_DRIFTED, RpcError, STATE_UNUSABLE,
};
use super::plan_registry::PlanLookupError;
use crate::cmd::input_error::CmdInputError;
use crate::mount::MountRefusal;

/// Map a handler's `eyre::Report` onto a JSON-RPC error envelope.
///
/// `method` is the wire-name of the failing RPC (`"metadata.set"`,
/// `"corpus.rebuild"`, ...), used only to label the residual
/// [`INTERNAL_ERROR`] message.
///
/// An error the walk recognises no type in still leaves through
/// [`rpc_from_problem`], so `data` is on the envelope unconditionally:
/// `retryable` alone, since such an error has neither evidence nor a
/// next step to name.
pub(crate) fn handler_err(method: &str, err: Report) -> RpcError {
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
        if let Some(e) = cause.downcast_ref::<EmbedError>() {
            return from_embed(e);
        }
        if let Some(e) = cause.downcast_ref::<QueryError>() {
            return from_query(e);
        }
        if let Some(e) = cause.downcast_ref::<VectorsError>() {
            return from_vectors(e);
        }
        if let Some(e) = cause.downcast_ref::<CatalogError>() {
            return from_catalog(e);
        }
        if let Some(e) = cause.downcast_ref::<CorpusError>() {
            return from_corpus(e);
        }
    }
    rpc_from_problem(
        INTERNAL_ERROR,
        Problem::new(format!("{method} failed: {err:#}")),
    )
}

/// Map a directly-held [`OpsError`] without an `eyre` round-trip: the
/// read proxies and the MCP tools get the typed error straight from
/// `bookrack_ops` and classify it here, with the same arms the write
/// side reaches through [`handler_err`]. The MCP front end re-wraps the
/// returned envelope in its own error type without re-deciding the
/// code, so one typed error takes one code on both surfaces.
pub fn ops_err(e: OpsError) -> RpcError {
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

/// Map a change to the mounted set onto the corresponding wire code.
///
/// The `match` is exhaustive: each variant already knows whether it is
/// caller input, a refusal the caller can repair, or a fault, so the
/// only decision a new one carries is which code it takes — and that
/// decision should not have a silent default. The refusals carry their
/// own three-part [`Problem`], so nothing is worded here.
pub(crate) fn mount_err(e: MountRefusal) -> RpcError {
    match e {
        MountRefusal::Unresolvable(e) => from_config(&e),
        MountRefusal::Registry(e) => from_registry(&e),
        MountRefusal::Refused { problem, .. } => rpc_from_problem(INVALID_PARAMS, problem),
        MountRefusal::RootLocked { problem, .. } => rpc_from_problem(BACKEND_UNAVAILABLE, problem),
        MountRefusal::BringUp(err) => rpc_from_problem(
            INTERNAL_ERROR,
            Problem::from_error_chain(err.as_ref() as &dyn std::error::Error),
        ),
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
    rpc_from_problem(
        INVALID_PARAMS,
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

/// Map an embed-backend failure onto the code its judgement implies.
///
/// The split follows `retryable`, not `EmbedError::is_transient()`:
/// the two ask different questions and are expected to disagree on an
/// overloaded server. The `match` reads the variant rather than the
/// rendered `Problem`, so classification does not depend on the
/// presentation layer it is supposed to precede.
///
/// The wildcard is forced: [`EmbedError`] is `#[non_exhaustive]` and
/// this is a downstream crate, so an exhaustive `match` does not
/// compile here. The guard against a new variant landing silently on
/// [`INTERNAL_ERROR`] lives beside the type instead, in
/// `bookrack-embed`'s own tests.
fn from_embed(e: &EmbedError) -> RpcError {
    use EmbedError::*;
    let code = match e {
        // The model name comes from the operator's index profile, and
        // the repair is `ollama pull`.
        ModelNotFound { .. } => INVALID_PARAMS,
        Unreachable(_) | Overloaded { .. } => BACKEND_UNAVAILABLE,
        // The request body is assembled here, so the operator cannot
        // have written either of these.
        BadRequest { .. } | MalformedResponse(_) => INTERNAL_ERROR,
        _ => INTERNAL_ERROR,
    };
    rpc_from_problem(code, e.explain())
}

/// Map a pipeline wrapper onto its wire code, delegating every variant
/// that carries a leaf to that leaf's classifier. `Rerank` stays here:
/// `RerankError` writes its own wording but has no caller-input
/// variant, so the wrapper's residual code is already right for it.
fn from_ops(e: &OpsError) -> RpcError {
    use OpsError::*;
    match e {
        Query(e) => return from_query(e),
        Catalog(e) => return from_catalog(e),
        Corpus(e) => return from_corpus(e),
        Vectors(e) => return from_vectors(e),
        _ => {}
    }
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

/// `Extract`, `Envelope`, and `Io` are not delegated: the extraction
/// refusals that look like caller input (`UnsupportedFormat`,
/// `DrmProtected`) are intercepted earlier on the write path by the
/// queue's extension check and by `NeedsOcr`, so a second judgement
/// here would be a second place for the two to drift apart.
fn from_ingest(e: &IngestError) -> RpcError {
    use IngestError::*;
    match e {
        Embed(e) => return from_embed(e),
        Catalog(e) => return from_catalog(e),
        Corpus(e) => return from_corpus(e),
        Vectors(e) => return from_vectors(e),
        _ => {}
    }
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
    match e {
        Embed(e) => return from_embed(e),
        Catalog(e) => return from_catalog(e),
        Corpus(e) => return from_corpus(e),
        Vectors(e) => return from_vectors(e),
        _ => {}
    }
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

/// Map a query-layer wrapper onto its wire code. Every variant but the
/// two the query layer raises itself (`Search`, `EmptyProbe`) carries
/// a leaf and delegates to it; those two are faults in this binary.
fn from_query(e: &QueryError) -> RpcError {
    match e {
        QueryError::Embed(e) => from_embed(e),
        QueryError::Vectors(e) => from_vectors(e),
        QueryError::Corpus(e) => from_corpus(e),
        QueryError::Catalog(e) => from_catalog(e),
        QueryError::Search(_) | QueryError::EmptyProbe => {
            rpc_from_problem(INTERNAL_ERROR, e.explain())
        }
        // `QueryError` is `#[non_exhaustive]`; a variant this build does
        // not know is a fault until someone judges otherwise.
        _ => rpc_from_problem(INTERNAL_ERROR, e.explain()),
    }
}

/// Map a leaf `VectorsError` onto its wire code.
///
/// The type has no `Explain` impl, so the summary is the flattened
/// chain. The variants whose next step is "change what you passed"
/// are caller input; those whose next step is the operator's — a newer
/// build, a rebuild, a reset — take [`STATE_UNUSABLE`], since calling
/// them invalid params would be a lie of the same shape this line
/// rejected for `-32013` and `-32001`, and calling them internal would
/// hide a step that exists. Both kinds carry that step as the hint.
/// Everything else is a fault and stays on [`INTERNAL_ERROR`].
fn from_vectors(e: &VectorsError) -> RpcError {
    use VectorsError::*;
    let problem = Problem::from_error_chain(e);
    match e {
        UnknownAnnKind(_) => rpc_from_problem(
            STATE_UNUSABLE,
            problem.hint(
                "Run a bookrack build that knows this ANN kind, or run `bookrack vectors \
                 rebuild` to rewrite the index and its `vectors_meta.json` sidecar.",
            ),
        ),
        DimensionMismatch { expected, .. } => rpc_from_problem(
            STATE_UNUSABLE,
            problem.hint(format!(
                "Run `bookrack vectors reset` to re-embed every chunk at the configured \
                 model's dimension; the store was built at {expected}."
            )),
        ),
        ReaderTooOld { required, .. } => rpc_from_problem(
            STATE_UNUSABLE,
            problem.hint(format!(
                "Run a bookrack build at reader version v{required} or newer."
            )),
        ),
        BuildOnBruteForceKind => rpc_from_problem(
            INVALID_PARAMS,
            problem.hint(
                "Brute-force search has no index to build. Choose an ANN kind in the \
                 index profile, or drop the index instead of building one.",
            ),
        ),
        MissingPqParam(param) => rpc_from_problem(
            INVALID_PARAMS,
            problem.hint(format!(
                "Set `{param}` in the index profile; an IvfPq build does not run without it."
            )),
        ),
        IvfPqQuantizationTooCoarse {
            dim,
            num_sub_vectors: _,
        } => rpc_from_problem(
            INVALID_PARAMS,
            problem.hint(format!(
                "Raise `num_sub_vectors` to at least {} (the embedding dimension divided by \
                 8) in the index profile, or choose a kind other than IvfPq.",
                dim.div_ceil(8)
            )),
        ),
        _ => rpc_from_problem(INTERNAL_ERROR, problem),
    }
}

/// Map a leaf `CatalogError` onto its wire code. Same contract as
/// [`from_vectors`]: no `Explain` on the type, a hint only beside a
/// caller-input code.
fn from_catalog(e: &CatalogError) -> RpcError {
    let problem = Problem::from_error_chain(e);
    match e {
        CatalogError::SchemaTooNew { expected, .. } => rpc_from_problem(
            STATE_UNUSABLE,
            problem.hint(format!(
                "Run a newer bookrack build (this one reads catalog schemas up to \
                 v{expected}), or restore the catalog from a backup this build wrote."
            )),
        ),
        CatalogError::ReaderTooOld { required, .. } => rpc_from_problem(
            STATE_UNUSABLE,
            problem.hint(format!(
                "Run a bookrack build at reader version v{required} or newer."
            )),
        ),
        CatalogError::DerivedFromConflict { .. } => rpc_from_problem(
            INVALID_PARAMS,
            problem.hint(
                "A derived text keeps the source it was first registered against. Register \
                 it against that source, or remove the intake and ingest it again for the \
                 new one.",
            ),
        ),
        _ => rpc_from_problem(INTERNAL_ERROR, problem),
    }
}

/// Map a leaf `CorpusError` onto its wire code. Same contract as
/// [`from_vectors`].
fn from_corpus(e: &CorpusError) -> RpcError {
    let problem = Problem::from_error_chain(e);
    match e {
        CorpusError::SchemaMismatch { .. } | CorpusError::IndexNotStamped => rpc_from_problem(
            STATE_UNUSABLE,
            problem.hint(
                "Run `bookrack corpus rebuild` to rewrite the corpus at this build's schema \
                 and stamps.",
            ),
        ),
        CorpusError::IndexStampMismatch { key, .. } => rpc_from_problem(
            STATE_UNUSABLE,
            problem.hint(format!(
                "Run `bookrack stamps reconcile` to see what the `{key}` stamp invalidates, \
                 then the refresh command it names."
            )),
        ),
        CorpusError::ReaderTooOld { required, .. } => rpc_from_problem(
            STATE_UNUSABLE,
            problem.hint(format!(
                "Run a bookrack build at reader version v{required} or newer."
            )),
        ),
        CorpusError::InvalidIntakeId(_) => rpc_from_problem(
            INVALID_PARAMS,
            problem.hint("Pass a positive intake id; `bookrack list` prints them."),
        ),
        _ => rpc_from_problem(INTERNAL_ERROR, problem),
    }
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
        let rpc = handler_err("metadata.set", err);
        assert_eq!(rpc.code, INVALID_PARAMS);
        assert!(rpc.message.contains("42"));
    }

    #[test]
    fn ops_unknown_field_is_invalid_params() {
        let err: Report = OpsError::UnknownMetadataField {
            field: "no_such_field".into(),
            editable: vec![String::from("title")],
        }
        .into();
        let rpc = handler_err("metadata.set", err);
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
        let rpc = handler_err("corpus.rebuild", err);
        assert_eq!(rpc.code, INVALID_PARAMS);
    }

    #[test]
    fn glean_needs_ocr_is_invalid_params() {
        let err: Report = GleanError::NeedsOcr {
            reason: "no text layer".into(),
        }
        .into();
        let rpc = handler_err("papers.corpus_rebuild", err);
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
        let rpc = handler_err("library.set_default", err);
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
        let rpc = handler_err("library.search", err);
        assert_eq!(rpc.code, BACKEND_UNAVAILABLE);
        let wire = serde_json::to_string(&rpc).expect("serialize");
        assert!(wire.contains("boom"), "root cause lost: {wire}");
    }

    /// A command that talks to the embedder folds the failure through
    /// `?` and `.context()`, so the arm has to find the type below the
    /// wraps. An absent model is the operator's own configuration —
    /// the repair is `ollama pull`, and the hint that names it has to
    /// reach the wire for the code to be worth anything.
    #[test]
    fn a_bare_model_not_found_is_caller_input_and_keeps_its_hint() {
        let inner: Result<(), bookrack_embed::EmbedError> =
            Err(bookrack_embed::EmbedError::ModelNotFound {
                model: "test-model".into(),
                reason: "model not found, try pulling it first".into(),
            });
        let err: Report = inner
            .context("probe embedding dimension")
            .context("stamps.reconcile")
            .unwrap_err();
        let rpc = handler_err("stamps.reconcile", err);
        assert_eq!(rpc.code, INVALID_PARAMS);
        let data: bookrack_core::ProblemData =
            serde_json::from_value(rpc.data.expect("data slot filled")).expect("ProblemData");
        assert!(
            data.hint.expect("hint").contains("ollama pull test-model"),
            "the repair must reach the operator"
        );
    }

    /// An unreachable backend is neither caller input nor a bug in this
    /// binary: the same call may succeed once Ollama is up, which is
    /// what `retryable` says and what the dedicated code carries.
    #[test]
    fn a_bare_unreachable_backend_is_backend_unavailable_and_retryable() {
        let err: Report =
            bookrack_embed::EmbedError::Unreachable("connection refused".into()).into();
        let rpc = handler_err("stamps.reconcile", err);
        assert_eq!(rpc.code, BACKEND_UNAVAILABLE);
        let data: bookrack_core::ProblemData =
            serde_json::from_value(rpc.data.expect("data slot filled")).expect("ProblemData");
        assert!(data.retryable);
        assert!(data.hint.expect("hint").contains("BOOKRACK_OLLAMA_URL"));
    }

    /// The same embed failure reached through a pipeline wrapper takes
    /// the same code as the bare form. Asserted against the literal
    /// code rather than against the bare form's code: before the
    /// wrappers delegated, both were `-32603`, so comparing the two
    /// would have passed while the classification was wrong.
    #[test]
    fn a_wrapped_model_not_found_takes_the_same_code_as_a_bare_one() {
        let wrapped: Vec<Report> = vec![
            IngestError::Embed(bookrack_embed::EmbedError::ModelNotFound {
                model: "test-model".into(),
                reason: "model not found".into(),
            })
            .into(),
            GleanError::Embed(bookrack_embed::EmbedError::ModelNotFound {
                model: "test-model".into(),
                reason: "model not found".into(),
            })
            .into(),
            OpsError::Query(bookrack_query::QueryError::Embed(
                bookrack_embed::EmbedError::ModelNotFound {
                    model: "test-model".into(),
                    reason: "model not found".into(),
                },
            ))
            .into(),
        ];
        for err in wrapped {
            let label = format!("{err:#}");
            let rpc = handler_err("vectors.reembed", err);
            assert_eq!(rpc.code, INVALID_PARAMS, "{label}");
        }
    }

    /// The request body an embed call sends is assembled by this
    /// binary, so a 4xx that is not an absent model is a fault on this
    /// side. It stays in the residual bucket, and the delegate must not
    /// sweep it into the caller-input one.
    #[test]
    fn a_bad_request_from_the_backend_stays_internal() {
        for e in [
            bookrack_embed::EmbedError::BadRequest {
                status: 400,
                body: "malformed input".into(),
            },
            bookrack_embed::EmbedError::MalformedResponse("not json".into()),
        ] {
            let label = format!("{e:?}");
            let rpc = handler_err("stamps.reconcile", e.into());
            assert_eq!(rpc.code, INTERNAL_ERROR, "{label}");
        }
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

    /// Pins the recursion itself: a leaf variant two wrappers deep takes
    /// the code its own classifier assigns. Asserted against the literal
    /// code — before the wrappers delegated, every leaf under them was
    /// `-32603`, so comparing wrapped to bare would have passed while
    /// both were wrong.
    #[test]
    fn a_wrapped_leaf_variant_takes_its_own_code_through_every_wrapper() {
        use bookrack_vectors::VectorsError;
        let cases: Vec<(&str, Report)> = vec![
            (
                "ingest > vectors",
                IngestError::Vectors(VectorsError::MissingPqParam("num_sub_vectors")).into(),
            ),
            (
                "glean > vectors",
                GleanError::Vectors(VectorsError::BuildOnBruteForceKind).into(),
            ),
            (
                "ops > query > vectors",
                OpsError::Query(bookrack_query::QueryError::Vectors(
                    VectorsError::MissingPqParam("num_sub_vectors"),
                ))
                .into(),
            ),
            (
                "ops > corpus",
                OpsError::Corpus(bookrack_corpus::CorpusError::InvalidIntakeId(-1)).into(),
            ),
            (
                "ingest > corpus",
                IngestError::Corpus(bookrack_corpus::CorpusError::InvalidIntakeId(-1)).into(),
            ),
            (
                "glean > catalog",
                GleanError::Catalog(bookrack_catalog::CatalogError::DerivedFromConflict {
                    intake_id: 7,
                    existing: "aaa".into(),
                    requested: "bbb".into(),
                })
                .into(),
            ),
        ];
        for (label, err) in cases {
            let rpc = handler_err("vectors.rebuild", err);
            assert_eq!(rpc.code, INVALID_PARAMS, "{label}: {}", rpc.message);
        }
    }

    /// A leaf error a command raises without any pipeline wrapper around
    /// it must be recognised by the walk, not swept into the residual
    /// channel.
    #[test]
    fn a_bare_leaf_variant_is_recognised_by_the_walk() {
        let inner: Result<(), bookrack_vectors::VectorsError> = Err(
            bookrack_vectors::VectorsError::MissingPqParam("num_sub_vectors"),
        );
        let err: Report = inner
            .context("build the ANN index")
            .context("vectors.rebuild")
            .unwrap_err();
        let rpc = handler_err("vectors.rebuild", err);
        assert_eq!(rpc.code, INVALID_PARAMS, "{}", rpc.message);
        assert!(
            !rpc.message.contains("vectors.rebuild failed:"),
            "the residual channel must not have handled it: {}",
            rpc.message
        );

        let err: Report = bookrack_corpus::CorpusError::InvalidIntakeId(-1).into();
        assert_eq!(handler_err("ingest.submit", err).code, INVALID_PARAMS);
    }

    /// Every variant the classifier judges caller input carries a next
    /// step on the wire. The leaf types write no wording of their own,
    /// so the hint is the classifier's, keyed on the code it chose.
    #[test]
    fn every_caller_input_leaf_variant_carries_a_hint() {
        use bookrack_vectors::VectorsError;
        let cases: Vec<(&str, Report)> = vec![
            (
                "BuildOnBruteForceKind",
                VectorsError::BuildOnBruteForceKind.into(),
            ),
            (
                "MissingPqParam",
                VectorsError::MissingPqParam("num_sub_vectors").into(),
            ),
            (
                "IvfPqQuantizationTooCoarse",
                VectorsError::IvfPqQuantizationTooCoarse {
                    dim: 1024,
                    num_sub_vectors: 16,
                }
                .into(),
            ),
            (
                "InvalidIntakeId",
                bookrack_corpus::CorpusError::InvalidIntakeId(0).into(),
            ),
            (
                "DerivedFromConflict",
                bookrack_catalog::CatalogError::DerivedFromConflict {
                    intake_id: 7,
                    existing: "aaa".into(),
                    requested: "bbb".into(),
                }
                .into(),
            ),
        ];
        for (label, err) in cases {
            let rpc = handler_err("vectors.rebuild", err);
            assert_eq!(rpc.code, INVALID_PARAMS, "{label}");
            let data: bookrack_core::ProblemData =
                serde_json::from_value(rpc.data.expect("data slot filled")).expect("ProblemData");
            let hint = data.hint.unwrap_or_default();
            assert!(
                !hint.trim().is_empty(),
                "{label}: caller input without a next step"
            );
            assert!(!data.retryable, "{label}");
        }
    }

    /// Delegation must not widen the caller-input bucket: a leaf fault
    /// stays `-32603`. The one visible trace of the delegation is the
    /// wrapper's own `Display` ("catalog error: ") leaving the summary,
    /// because the leaf is now rendered from its own chain.
    #[test]
    fn a_wrapped_leaf_fault_stays_internal_and_drops_the_wrapper_prefix() {
        let err: Report = OpsError::Catalog(bookrack_catalog::CatalogError::Io(
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "backup dir read-only"),
        ))
        .into();
        let rpc = handler_err("metadata.set", err);
        assert_eq!(rpc.code, INTERNAL_ERROR);
        assert!(
            !rpc.message.starts_with("catalog error"),
            "wrapper Display leaked onto the wire: {}",
            rpc.message
        );
        assert!(
            rpc.message.contains("backup dir read-only"),
            "{}",
            rpc.message
        );
    }

    /// Every store variant whose next step is the operator's — a newer
    /// build, a rebuild, a reset — takes the state code, bare or
    /// wrapped, and names that step. Written out rather than derived
    /// from the `from_*` functions, which are what is under test.
    #[test]
    fn every_unusable_state_variant_takes_the_state_code_with_a_next_step() {
        use bookrack_catalog::CatalogError;
        use bookrack_corpus::CorpusError;
        use bookrack_vectors::VectorsError;
        let cases: Vec<(&str, Report)> = vec![
            (
                "catalog SchemaTooNew (bare)",
                CatalogError::SchemaTooNew {
                    found: 99,
                    expected: 17,
                }
                .into(),
            ),
            (
                "catalog ReaderTooOld (ops)",
                OpsError::Catalog(CatalogError::ReaderTooOld {
                    required: 9,
                    current: 3,
                })
                .into(),
            ),
            (
                "corpus SchemaMismatch (ingest)",
                IngestError::Corpus(CorpusError::SchemaMismatch {
                    found: "v0".into(),
                    expected: 4,
                })
                .into(),
            ),
            (
                "corpus ReaderTooOld (glean)",
                GleanError::Corpus(CorpusError::ReaderTooOld {
                    required: 9,
                    current: 3,
                })
                .into(),
            ),
            (
                "corpus IndexNotStamped (bare)",
                CorpusError::IndexNotStamped.into(),
            ),
            (
                "corpus IndexStampMismatch (ops > query)",
                OpsError::Query(bookrack_query::QueryError::Corpus(
                    CorpusError::IndexStampMismatch {
                        key: "embed_model",
                        found: "a".into(),
                        expected: "b".into(),
                    },
                ))
                .into(),
            ),
            (
                "vectors UnknownAnnKind (ops)",
                OpsError::Vectors(VectorsError::UnknownAnnKind("hyperspace".into())).into(),
            ),
            (
                "vectors DimensionMismatch (ingest)",
                IngestError::Vectors(VectorsError::DimensionMismatch {
                    got: 768,
                    expected: 1024,
                })
                .into(),
            ),
            (
                "vectors ReaderTooOld (bare)",
                VectorsError::ReaderTooOld {
                    required: 9,
                    current: 3,
                }
                .into(),
            ),
        ];
        for (label, err) in cases {
            let rpc = handler_err("vectors.rebuild", err);
            assert_eq!(rpc.code, STATE_UNUSABLE, "{label}: {}", rpc.message);
            let data: bookrack_core::ProblemData =
                serde_json::from_value(rpc.data.expect("data slot filled")).expect("ProblemData");
            assert!(
                !data.hint.unwrap_or_default().trim().is_empty(),
                "{label}: a state code without the operator's next step"
            );
            assert!(!data.retryable, "{label}");
        }
    }

    /// The two read-shape refusals the read proxies used to hand-pick
    /// are caller input in the shared classifier too, so routing the
    /// proxies through it does not demote them to `-32603`.
    #[test]
    fn read_shape_refusals_are_caller_input_through_ops_err() {
        for e in [
            OpsError::NotALeaf { node_id: 7 },
            OpsError::NotOrganizing { node_id: 7 },
        ] {
            let label = format!("{e:?}");
            let rpc = ops_err(e);
            assert_eq!(rpc.code, INVALID_PARAMS, "{label}");
            assert!(rpc.message.contains("node 7"), "{label}: {}", rpc.message);
        }
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
}
