// SPDX-License-Identifier: Apache-2.0

//! Judge an operation's failure once, for every transport.
//!
//! A failure reaches a caller over the control socket or over MCP, and
//! the caller has to learn the same thing from either: whether its own
//! input was refused, whether a store is in a state the operator has to
//! change, whether a backend did not answer and the call may succeed
//! later, or whether this is a fault on the serving side. This module
//! makes that judgement — an [`ErrorKind`] — and words the failure as a
//! three-part [`Problem`]; each transport only maps the kind onto its
//! own code. Nothing here knows a wire code.
//!
//! [`classify_report`] walks an `eyre` cause chain and stops at the
//! first cause of a type this module reads. The types come in two
//! tiers. The pipeline wrappers — [`OpsError`], [`IngestError`],
//! [`GleanError`], [`RegistryError`] — are read by
//! [`classify_ops_cause`]; the store and backend leaves they fold in —
//! [`EmbedError`], [`QueryError`], [`VectorsError`], [`CatalogError`],
//! [`CorpusError`], [`RefsError`] — by [`classify_leaf_cause`]. A host
//! with failure types of its own walks the chain itself and asks the
//! two per-cause entries in turn, so the outermost cause it recognises
//! decides, whichever tier it belongs to.
//!
//! Classification is recursive: a wrapper's `from_*` delegates every
//! variant that holds a leaf to that leaf's own `from_*`, so a leaf
//! takes the same kind whether a command raised it bare or a wrapper
//! (or two — `OpsError::Query(QueryError::Vectors(..))`) carried it.
//! The one shape the recursion cannot see through is
//! `OpsError::Other(eyre::Report)`: the report inside is opaque to the
//! `match`, so whatever typed error it holds takes the residual kind,
//! and a `warn` event (`unclassified ops error`) records that nothing
//! judged it. A variant a `from_*` judged a fault is a verdict and is
//! not logged.
//!
//! Wording is not written here. Each error renders itself through
//! [`bookrack_core::Explain`]; a type with no `Explain` impl takes
//! [`Problem::from_error_chain`], whose summary is the flattened cause
//! chain — `Display` on a wrapper variant prints only its own text
//! ("query error"), so a bare `to_string()` here would drop the root
//! cause at the process boundary. `scripts/error-boundary-check.sh`
//! enforces that.
//!
//! The exception to "wording is not written here" is the hint on a
//! leaf the classifier judges caller input or an unusable state. The
//! leaf store types write no wording of their own, and a kind without a
//! next step is an empty promise, so the `from_*` for those types
//! attaches the step beside the kind it chose. That is the classifier
//! stating what its own judgement implies, not the leaf inventing facts
//! about itself: the summary stays the leaf's flattened chain.

use std::error::Error;
use std::ops::ControlFlow;
use std::path::Path;

use bookrack_catalog::CatalogError;
use bookrack_config::ConfigError;
use bookrack_core::{Explain, Problem};
use bookrack_corpus::CorpusError;
use bookrack_embed::EmbedError;
use bookrack_query::QueryError;
use bookrack_refs::RefsError;
use bookrack_rerank::RerankError;
use bookrack_vectors::{TrainingLimit, VectorsError};
use eyre::Report;

use crate::OpsError;
use crate::dto::UnknownFilterValue;
use crate::pipeline::book::IngestError;
use crate::pipeline::paper::GleanError;
use crate::reads::reference::ReferenceError;
use crate::registry::RegistryError;

/// What a failure means for the caller, whichever transport carries it.
///
/// Each transport maps every kind onto a code of its own, with an
/// exhaustive `match`: a new kind is a decision for each of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The caller's input is refused; the same call resent unchanged
    /// fails the same way.
    InvalidParams,
    /// A fault on the serving side, or a failure nothing has judged.
    Internal,
    /// Another write holds the library; the same call may succeed once
    /// it ends.
    Busy,
    /// The call names, or fails to name, a library it cannot be routed
    /// to.
    InvalidLibrary,
    /// A backend the call depends on did not answer; the same call may
    /// succeed once it does.
    BackendUnavailable,
    /// A store is in a state this build cannot serve until the operator
    /// changes it.
    StateUnusable,
}

/// A failure's [`ErrorKind`] and its three-part wording.
///
/// Built only in this module: a judgement made anywhere else would be a
/// second place for the two transports to drift apart.
/// `scripts/error-boundary-check.sh` refuses the struct literal outside
/// this file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClassifiedError {
    /// What the failure means for the caller.
    pub kind: ErrorKind,
    /// The summary, detail, hint, and retryable flag the caller reads.
    pub problem: Problem,
}

impl ClassifiedError {
    fn new(kind: ErrorKind, problem: Problem) -> Self {
        Self { kind, problem }
    }
}

/// The judgement of `err`'s chain when a cause on it is of a type this
/// module reads, and `None` when none is, or when the first one it
/// reads holds no judgement.
///
/// The walk takes the causes outermost first and stops at the first
/// one either per-cause entry reads, so a chain judged here is judged
/// the same as by a host that walks it itself and asks
/// [`classify_ops_cause`] and [`classify_leaf_cause`] in turn.
// Parallel implementation: frontend-error-map.
pub fn classify_report(err: &Report) -> Option<ClassifiedError> {
    for cause in err.chain() {
        if let Some(classified) = classify_ops_cause(cause) {
            return Some(classified);
        }
        if let ControlFlow::Break(found) = classify_leaf_cause(cause) {
            return found;
        }
    }
    None
}

/// The judgement of one cause when it is a pipeline wrapper or a
/// registry refusal, and `None` for every other type. Every variant of
/// these types is judged, so a match always classifies.
pub fn classify_ops_cause(cause: &(dyn Error + 'static)) -> Option<ClassifiedError> {
    if let Some(e) = cause.downcast_ref::<OpsError>() {
        return Some(from_ops(e));
    }
    if let Some(e) = cause.downcast_ref::<IngestError>() {
        return Some(from_ingest(e));
    }
    if let Some(e) = cause.downcast_ref::<GleanError>() {
        return Some(from_glean(e));
    }
    if let Some(e) = cause.downcast_ref::<RegistryError>() {
        return Some(from_registry(e));
    }
    None
}

/// The judgement of one cause when it is a store or backend leaf.
///
/// `Continue` when the cause is of no leaf type, so a chain walk moves
/// on to the next cause. `Break` when it is one: with the judgement, or
/// with `None` for a reference store variant that has no next step to
/// name, which ends the walk unclassified so the chain keeps the
/// context its caller attached.
pub fn classify_leaf_cause(cause: &(dyn Error + 'static)) -> ControlFlow<Option<ClassifiedError>> {
    if let Some(e) = cause.downcast_ref::<EmbedError>() {
        return ControlFlow::Break(Some(from_embed(e)));
    }
    if let Some(e) = cause.downcast_ref::<QueryError>() {
        return ControlFlow::Break(Some(from_query(e)));
    }
    if let Some(e) = cause.downcast_ref::<VectorsError>() {
        return ControlFlow::Break(Some(from_vectors(e)));
    }
    if let Some(e) = cause.downcast_ref::<CatalogError>() {
        return ControlFlow::Break(Some(from_catalog(e)));
    }
    if let Some(e) = cause.downcast_ref::<CorpusError>() {
        return ControlFlow::Break(Some(from_corpus(e)));
    }
    if let Some(e) = cause.downcast_ref::<RefsError>() {
        return ControlFlow::Break(from_refs(e));
    }
    ControlFlow::Continue(())
}

/// The three-part reason a library whose bring-up failed with `err`
/// can only be replaced, not repaired in place: a catalog on the chain
/// that holds retired labels. `None` for every other error.
///
/// `library` is the name the registry carries the library under, or
/// `None` for a root selected by path; with a name, the hint names it.
pub fn retired_labels_reason(library: Option<&str>, err: &Report) -> Option<Problem> {
    err.chain()
        .find_map(|cause| match cause.downcast_ref::<CatalogError>() {
            Some(CatalogError::RetiredLabels { path, hits }) => {
                Some(retired_labels_problem(library, path, hits))
            }
            _ => None,
        })
}

/// The three-part reading of a catalog at `path` that is behind this
/// build and holds rows carrying retired labels.
///
/// The summary states the fact, the detail counts each label, and the
/// hint is the removal of the whole library. With `library`, the hint
/// names it; without, it points at the data root and at the listing
/// that carries the name of a library registered there.
fn retired_labels_problem(
    library: Option<&str>,
    path: &Path,
    hits: &[bookrack_dbkit::RetiredHit],
) -> Problem {
    let counted = hits
        .iter()
        .map(|hit| format!("{hit}"))
        .collect::<Vec<String>>()
        .join(", ");
    let removal = match library {
        Some(name) => format!(
            "Remove the library with `bookrack libraries remove {name} --purge` and ingest \
             its books again, or keep a bookrack 0.14 binary for it."
        ),
        None => {
            let root = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(path);
            format!(
                "If `bookrack libraries list` shows a library at the root '{}', remove it with \
                 `bookrack libraries remove <name> --purge` and ingest its books again; or \
                 keep a bookrack 0.14 binary for it.",
                root.display()
            )
        }
    };
    Problem::new(format!(
        "catalog \"{}\" holds rows from the removed OCR intake path",
        path.display()
    ))
    .detail(format!(
        "The catalog was neither backed up nor migrated. Rows carrying a retired label: \
         {counted}."
    ))
    .hint(format!(
        "This build cannot open the catalog, and no step repairs it in place. {removal}"
    ))
}

/// Judge a reference read or write failure.
///
/// A request field outside what the call accepts, and an overlay key
/// outside the property catalog, are caller input; the message already
/// names the field or key. A store whose schema this build does not
/// read is a state the operator has to change, with the direction of
/// the mismatch deciding the hint. Everything else — a path that could
/// not be inspected, a damaged store, a query fault, a catalog that did
/// not parse — is a fault carrying its full cause chain.
pub fn from_reference(e: &ReferenceError) -> ClassifiedError {
    match e {
        ReferenceError::InvalidArgument(message) => {
            ClassifiedError::new(ErrorKind::InvalidParams, Problem::new(message.as_str()))
        }
        ReferenceError::UnknownOverlayProperty { .. } => {
            ClassifiedError::new(ErrorKind::InvalidParams, Problem::from_error_chain(e))
        }
        ReferenceError::Open {
            path,
            source: RefsError::SchemaTooNew { found, supported },
        } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            reference_schema_problem(path, *found, *supported).hint(
                "This build is older than the store. Run the build that wrote the store, \
                 or a newer one.",
            ),
        ),
        ReferenceError::Open {
            path,
            source: RefsError::SchemaTooOld { found, supported },
        } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            reference_schema_problem(path, *found, *supported).hint(
                "Run `bookrack distill build` with this build, or start the daemon once \
                 with `bookrack run`: either one migrates the store to this build's schema.",
            ),
        ),
        ReferenceError::Open {
            path,
            source: RefsError::ReaderTooOld { required, current },
        } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            Problem::new("cannot read the reference store")
                .detail(format!(
                    "The store at {} requires reader version {required}; this build is at \
                     reader version {current}.",
                    path.display()
                ))
                .hint(format!(
                    "Run a bookrack build at reader version v{required} or newer."
                )),
        ),
        ReferenceError::Open { .. }
        | ReferenceError::Store(_)
        | ReferenceError::Io { .. }
        | ReferenceError::Catalog(_) => {
            ClassifiedError::new(ErrorKind::Internal, Problem::from_error_chain(e))
        }
    }
}

/// The summary and detail of a reference store at a schema this build
/// does not read; the caller adds the hint its direction calls for.
fn reference_schema_problem(path: &Path, found: i64, supported: i64) -> Problem {
    Problem::new("cannot read the reference store").detail(format!(
        "The store at {} is at schema {found}; this build reads schema {supported}.",
        path.display()
    ))
}

/// Judge a refused filter value as caller input.
///
/// The accepted set travels with the refusal rather than being spelled
/// out here, so a vocabulary that gains a value cannot leave a stale
/// list behind.
pub fn from_unknown_filter(unknown: &UnknownFilterValue) -> ClassifiedError {
    ClassifiedError::new(
        ErrorKind::InvalidParams,
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

/// Judge an embed-backend failure.
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
/// [`ErrorKind::Internal`] lives beside the type instead, in
/// `bookrack-embed`'s own tests.
fn from_embed(e: &EmbedError) -> ClassifiedError {
    use EmbedError::*;
    let kind = match e {
        // The model name comes from the operator's index profile, and
        // the repair is `ollama pull`.
        ModelNotFound { .. } => ErrorKind::InvalidParams,
        Unreachable(_) | Overloaded { .. } => ErrorKind::BackendUnavailable,
        // The request body is assembled here, so the operator cannot
        // have written either of these.
        BadRequest { .. } | MalformedResponse(_) => ErrorKind::Internal,
        // The backend broke its response contract; the hint still names
        // the operator's way around it.
        UnusableVector { .. } => ErrorKind::Internal,
        _ => ErrorKind::Internal,
    };
    ClassifiedError::new(kind, e.explain())
}

/// Judge an [`OpsError`], delegating every variant that carries a leaf
/// or a backend failure to that type's classifier.
pub fn from_ops(e: &OpsError) -> ClassifiedError {
    use OpsError::*;
    match e {
        Query(e) => return from_query(e),
        Catalog(e) => return from_catalog(e),
        Corpus(e) => return from_corpus(e),
        Vectors(e) => return from_vectors(e),
        Rerank(e) => return from_rerank(e),
        // The report inside is opaque to this `match`, so nothing has
        // judged what it holds: the fault kind is a fallback, not a
        // verdict, and the serving side keeps a record of it.
        Other(_) => {
            let problem = e.explain();
            tracing::warn!(error = %problem.summary, "unclassified ops error");
            return ClassifiedError::new(ErrorKind::Internal, problem);
        }
        _ => {}
    }
    let kind = match e {
        IntakeNotFound { .. }
        | UnknownMetadataField { .. }
        | UnknownContributorRole { .. }
        | ContributorNotFound { .. }
        | NodeNotFound { .. }
        | NotALeaf { .. }
        | NotOrganizing { .. }
        | NoPassagesForKind { .. }
        | SourceNotArchived { .. }
        | UnknownAuditProfile { .. } => ErrorKind::InvalidParams,
        WriteBusy(_) => ErrorKind::Busy,
        _ => ErrorKind::Internal,
    };
    ClassifiedError::new(kind, e.explain())
}

/// Judge a reranker failure. A server that did not answer, or answered
/// that it is overloaded or still loading its model, is a backend the
/// same call may reach later; a request it refused and a reply that
/// does not parse are faults, since this binary assembles the request
/// and the backend broke its response contract. The wording is the
/// reranker's own.
///
/// The wildcard is forced: [`RerankError`] is `#[non_exhaustive]` and
/// this is a downstream crate. The guard against a new variant landing
/// silently on [`ErrorKind::Internal`] lives beside the type instead,
/// in `bookrack-rerank`'s own tests.
fn from_rerank(e: &RerankError) -> ClassifiedError {
    use RerankError::*;
    let kind = match e {
        Unreachable(_) | Overloaded { .. } => ErrorKind::BackendUnavailable,
        BadRequest { .. } | MalformedResponse(_) => ErrorKind::Internal,
        _ => ErrorKind::Internal,
    };
    ClassifiedError::new(kind, e.explain())
}

/// `Extract`, `Envelope`, and `Io` are not delegated: the extraction
/// refusals that look like caller input (`UnsupportedFormat`,
/// `DrmProtected`) are intercepted earlier on the write path by the
/// queue's extension check and by `NeedsOcr`, so a second judgement
/// here would be a second place for the two to drift apart.
// Parallel implementation: control-error-map-pipeline.
fn from_ingest(e: &IngestError) -> ClassifiedError {
    use IngestError::*;
    match e {
        Embed(e) => return from_embed(e),
        Catalog(e) => return from_catalog(e),
        Corpus(e) => return from_corpus(e),
        Vectors(e) => return from_vectors(e),
        _ => {}
    }
    let kind = match e {
        EmptyExtraction
        | NeedsOcr { .. }
        | UnknownIntake(_)
        | MissingEnvelope(_)
        | EnvelopeMismatch(_)
        | IntakeNotEmbedded(_)
        | Rule(_)
        | RuleKindNotBook { .. }
        | RuleSourceNotPending { .. }
        | RuleSourceNotAFile { .. }
        | RuleSourceUnnumbered { .. }
        | RuleCoverage { .. } => ErrorKind::InvalidParams,
        _ => ErrorKind::Internal,
    };
    ClassifiedError::new(kind, e.explain())
}

// Parallel implementation: control-error-map-pipeline.
fn from_glean(e: &GleanError) -> ClassifiedError {
    use GleanError::*;
    match e {
        Embed(e) => return from_embed(e),
        Catalog(e) => return from_catalog(e),
        Corpus(e) => return from_corpus(e),
        Vectors(e) => return from_vectors(e),
        _ => {}
    }
    let kind = match e {
        NeedsOcr { .. }
        | UnknownIntake(_)
        | IntakeNotRebuildable(_)
        | MissingEnvelope(_)
        | EnvelopeMismatch(_) => ErrorKind::InvalidParams,
        _ => ErrorKind::Internal,
    };
    ClassifiedError::new(kind, e.explain())
}

/// Judge a query-layer wrapper. Every variant but the two the query
/// layer raises itself (`Search`, `EmptyProbe`) carries a leaf and
/// delegates to it; those two are faults in this binary.
fn from_query(e: &QueryError) -> ClassifiedError {
    match e {
        QueryError::Embed(e) => from_embed(e),
        QueryError::Vectors(e) => from_vectors(e),
        QueryError::Corpus(e) => from_corpus(e),
        QueryError::Catalog(e) => from_catalog(e),
        QueryError::Search(_) | QueryError::EmptyProbe => {
            ClassifiedError::new(ErrorKind::Internal, e.explain())
        }
        // `QueryError` is `#[non_exhaustive]`; a variant this build does
        // not know is a fault until someone judges otherwise.
        _ => ClassifiedError::new(ErrorKind::Internal, e.explain()),
    }
}

/// Judge a leaf `VectorsError`.
///
/// The type has no `Explain` impl, so the summary is the flattened
/// chain. The variants whose next step is "change what you passed"
/// are caller input; those whose next step is the operator's — a newer
/// build, a rebuild, a reset — are an unusable state, since calling
/// them caller input would misdirect the caller, and calling them a
/// fault would hide a step that exists. Both kinds carry that step as
/// the hint. Everything else is a fault.
fn from_vectors(e: &VectorsError) -> ClassifiedError {
    use VectorsError::*;
    let problem = Problem::from_error_chain(e);
    match e {
        UnknownAnnKind(_) => ClassifiedError::new(
            ErrorKind::StateUnusable,
            problem.hint(
                "Run a bookrack build that knows this ANN kind, or run `bookrack vectors \
                 rebuild` to rewrite the index and its `vectors_meta.json` sidecar.",
            ),
        ),
        DimensionMismatch { expected, .. } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            problem.hint(format!(
                "Run `bookrack vectors reset` to re-embed every chunk at the configured \
                 model's dimension; the store was built at {expected}."
            )),
        ),
        ReaderTooOld { required, .. } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            problem.hint(format!(
                "Run a bookrack build at reader version v{required} or newer."
            )),
        ),
        BuildOnBruteForceKind => ClassifiedError::new(
            ErrorKind::InvalidParams,
            problem.hint(
                "Brute-force search has no index to build. Choose an ANN kind in the \
                 index profile, or drop the index instead of building one.",
            ),
        ),
        MissingPqParam(param) => ClassifiedError::new(
            ErrorKind::InvalidParams,
            problem.hint(format!(
                "Set `{param}` in the index profile; an IvfPq build does not run without it."
            )),
        ),
        IvfPqQuantizationTooCoarse {
            dim,
            num_sub_vectors: _,
        } => ClassifiedError::new(
            ErrorKind::InvalidParams,
            problem.hint(format!(
                "Raise `num_sub_vectors` to at least {} (the embedding dimension divided by \
                 8) in the index profile, or choose a kind other than IvfPq.",
                dim.div_ceil(8)
            )),
        ),
        InvalidAnnConfig { field, .. } => ClassifiedError::new(
            ErrorKind::InvalidParams,
            problem.hint(format!(
                "Correct `{field}` in the index profile, or the `--{}` flag passed to \
                 `vectors rebuild`.",
                field.replace('_', "-")
            )),
        ),
        TooFewVectors {
            rows,
            required,
            limit,
            ..
        } => ClassifiedError::new(
            ErrorKind::InvalidParams,
            problem.hint(too_few_vectors_hint(*rows, *required, limit)),
        ),
        _ => ClassifiedError::new(ErrorKind::Internal, problem),
    }
}

/// The way forward from a store too small for the requested index:
/// only the routes that can succeed on `rows` vectors.
fn too_few_vectors_hint(rows: u64, required: u64, limit: &TrainingLimit) -> String {
    const KEEP_SCAN: &str = "To keep searching by exhaustive scan instead, run `bookrack vectors \
                             drop` (`bookrack papers vectors drop` for papers).";
    if rows == 0 {
        return format!(
            "The store holds no vectors yet; ingest before building an index. {KEEP_SCAN}"
        );
    }
    match limit {
        TrainingLimit::Partitions { .. } => format!(
            "Lower `num_partitions` to at most {rows}, or ingest more before rebuilding. \
             {KEEP_SCAN}"
        ),
        TrainingLimit::PqCodebook {
            num_bits,
            num_sub_vectors,
        } => {
            let narrower = *num_bits == 8
                && rows >= 16
                && num_sub_vectors.is_some_and(|nsv| nsv.is_multiple_of(2));
            let narrower = if narrower {
                " Setting `num_bits = 4` lowers the floor to 16 vectors."
            } else {
                ""
            };
            format!(
                "A {num_bits}-bit PQ codebook needs at least {required} vectors. Choose a kind \
                 without PQ, or ingest more before rebuilding.{narrower} {KEEP_SCAN}"
            )
        }
    }
}

/// Judge a leaf `CatalogError`. The type has no `Explain`: the summary
/// is its own text, and a hint is added for a state this build cannot
/// serve and for caller input. A catalog holding retired labels is the
/// exception, worded in three parts by [`retired_labels_problem`] with
/// no library to name.
fn from_catalog(e: &CatalogError) -> ClassifiedError {
    let problem = Problem::from_error_chain(e);
    match e {
        CatalogError::RetiredLabels { path, hits } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            retired_labels_problem(None, path, hits),
        ),
        CatalogError::SchemaTooNew { expected, .. } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            problem.hint(format!(
                "Run a newer bookrack build (this one reads catalog schemas up to \
                 v{expected}), or restore the catalog from a backup this build wrote."
            )),
        ),
        CatalogError::ReaderTooOld { required, .. } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            problem.hint(format!(
                "Run a bookrack build at reader version v{required} or newer."
            )),
        ),
        CatalogError::SchemaTooOld { .. } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            problem.hint(
                "Start the daemon once with `bookrack run`: mounting the library backs the \
                 catalog up and migrates it to this build's schema.",
            ),
        ),
        CatalogError::DerivedFromConflict { .. } => ClassifiedError::new(
            ErrorKind::InvalidParams,
            problem.hint(
                "A derived text keeps the source it was first registered against. Register \
                 it against that source, or remove the intake and ingest it again for the \
                 new one.",
            ),
        ),
        _ => ClassifiedError::new(ErrorKind::Internal, problem),
    }
}

/// The judgement of a `RefsError` that says the reference store is in
/// a state this build cannot serve, and `None` for every other one.
///
/// The summary is the leaf's own text, which names the store and both
/// versions; a context wrapped around it is not part of it. A variant
/// with no next step is left to the caller's residual channel, which
/// keeps that context.
fn from_refs(e: &RefsError) -> Option<ClassifiedError> {
    let hint = match e {
        RefsError::SchemaTooNew { supported, .. } => format!(
            "Run a newer bookrack build (this one reads reference store schemas up to \
             v{supported}), or restore reference.db from a backup this build wrote."
        ),
        RefsError::SchemaTooOld { .. } => String::from(
            "Run `bookrack distill build` with this build, or start the daemon once with \
             `bookrack run`: either one backs the reference store up and migrates it to \
             this build's schema.",
        ),
        RefsError::ReaderTooOld { required, .. } => {
            format!("Run a bookrack build at reader version v{required} or newer.")
        }
        _ => return None,
    };
    Some(ClassifiedError::new(
        ErrorKind::StateUnusable,
        Problem::from_error_chain(e).hint(hint),
    ))
}

/// Judge a leaf `CorpusError`. Same contract as [`from_vectors`].
fn from_corpus(e: &CorpusError) -> ClassifiedError {
    let problem = Problem::from_error_chain(e);
    match e {
        CorpusError::SchemaMismatch { .. } | CorpusError::IndexNotStamped => ClassifiedError::new(
            ErrorKind::StateUnusable,
            problem.hint(
                "Run `bookrack corpus rebuild` to rewrite the corpus at this build's \
                     schema and stamps.",
            ),
        ),
        CorpusError::IndexStampMismatch { key, .. } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            problem.hint(format!(
                "Run `bookrack stamps reconcile` to see what the `{key}` stamp invalidates, \
                 then the refresh command it names."
            )),
        ),
        CorpusError::ReaderTooOld { required, .. } => ClassifiedError::new(
            ErrorKind::StateUnusable,
            problem.hint(format!(
                "Run a bookrack build at reader version v{required} or newer."
            )),
        ),
        CorpusError::InvalidIntakeId(_) => ClassifiedError::new(
            ErrorKind::InvalidParams,
            problem.hint("Pass a positive intake id; `bookrack list` prints them."),
        ),
        _ => ClassifiedError::new(ErrorKind::Internal, problem),
    }
}

/// Judge a registry refusal. The enum is defined in this crate, so the
/// `match` is exhaustive despite `#[non_exhaustive]`: a new variant
/// does not compile until it is judged here.
pub fn from_registry(e: &RegistryError) -> ClassifiedError {
    let kind = match e {
        RegistryError::LibraryUnknown { .. }
        | RegistryError::UnnamedAmbiguous { .. }
        | RegistryError::LibraryUnmounted { .. } => ErrorKind::InvalidLibrary,
        RegistryError::Empty => ErrorKind::InvalidParams,
        RegistryError::AlreadyMounted { .. }
        | RegistryError::DefaultLockPoisoned
        | RegistryError::LibsLockPoisoned => ErrorKind::Internal,
    };
    ClassifiedError::new(kind, e.explain())
}

/// Judge a registry configuration failure. An unknown library named
/// against the on-disk registry is caller input naming a library;
/// every other registry fault is a fault on the serving side.
pub fn from_config(e: &ConfigError) -> ClassifiedError {
    let kind = match e {
        ConfigError::UnknownLibrary { .. } => ErrorKind::InvalidLibrary,
        _ => ErrorKind::Internal,
    };
    ClassifiedError::new(kind, Problem::from_error_chain(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_core::ProblemData;
    use eyre::WrapErr;

    /// The judgement of `err` through the whole-chain entry, which must
    /// read a type on it.
    fn classified(err: impl Into<Report>) -> ClassifiedError {
        let err = err.into();
        let label = format!("{err:#}");
        classify_report(&err).unwrap_or_else(|| panic!("no type read on: {label}"))
    }

    fn data(c: &ClassifiedError) -> &ProblemData {
        &c.problem.data
    }

    #[test]
    fn ops_intake_not_found_is_invalid_params() {
        let c = classified(OpsError::IntakeNotFound { intake_id: 42 });
        assert_eq!(c.kind, ErrorKind::InvalidParams);
        assert!(c.problem.summary.contains("42"), "{}", c.problem.summary);
    }

    #[test]
    fn ops_unknown_field_is_invalid_params() {
        let c = classified(OpsError::UnknownMetadataField {
            field: "no_such_field".into(),
            editable: vec![String::from("title")],
        });
        assert_eq!(c.kind, ErrorKind::InvalidParams);
        assert!(c.problem.summary.contains("no_such_field"));
    }

    #[test]
    fn ingest_unknown_intake_walks_context_chain() {
        let inner: Result<(), IngestError> = Err(IngestError::UnknownIntake(7));
        let err: Report = inner
            .context("rebuild step")
            .context("outer wrap")
            .unwrap_err();
        assert_eq!(classified(err).kind, ErrorKind::InvalidParams);
    }

    #[test]
    fn glean_needs_ocr_is_invalid_params() {
        let c = classified(GleanError::NeedsOcr {
            reason: "no text layer".into(),
        });
        assert_eq!(c.kind, ErrorKind::InvalidParams);
        assert!(c.problem.summary.contains("no text layer"));
    }

    /// The summary names only the name that did not resolve; the names
    /// that would are the next step, so they travel in the hint.
    #[test]
    fn registry_library_unknown_is_invalid_library() {
        let c = classified(RegistryError::LibraryUnknown {
            name: "ghost".into(),
            available: vec!["alpha".into(), "beta".into()],
        });
        assert_eq!(c.kind, ErrorKind::InvalidLibrary);
        assert_eq!(c.problem.summary, "no library named \"ghost\"");
        let hint = data(&c)
            .hint
            .as_deref()
            .expect("the available names are the next step");
        for available in ["alpha", "beta"] {
            assert!(hint.contains(available), "{hint}");
        }
        assert!(!data(&c).retryable);
    }

    /// `RegistryError` is `#[non_exhaustive]`, so a variant the mapping
    /// does not name falls through to the fault kind without a hint.
    /// The unnamed-route refusal is caller input with a next step and
    /// takes the library kind.
    #[test]
    fn unnamed_ambiguous_is_invalid_library_with_a_hint() {
        let c = classified(RegistryError::UnnamedAmbiguous {
            mounted: vec!["alpha".into(), "beta".into()],
            default: None,
        });
        assert_eq!(c.kind, ErrorKind::InvalidLibrary, "{}", c.problem.summary);
        assert_eq!(
            c.problem.summary,
            "cannot tell which library a call naming none is for"
        );
        let hint = data(&c)
            .hint
            .as_deref()
            .expect("naming a library is the next step");
        assert!(hint.contains("--library"), "{hint}");
        assert!(!data(&c).retryable);
    }

    /// A library bring-up skipped is caller input to fix: the library
    /// kind, with the skip's reason in the detail and its hint, which
    /// already closes with the mount command, as it is.
    #[test]
    fn registry_library_unmounted_is_invalid_library_with_the_mount_hint() {
        let c = classified(RegistryError::LibraryUnmounted {
            name: "beta".into(),
            reason: Box::new(
                Problem::new("cannot read the catalog")
                    .hint("Upgrade bookrack. Then run `bookrack libraries mount beta`."),
            ),
        });
        assert_eq!(c.kind, ErrorKind::InvalidLibrary, "{}", c.problem.summary);
        let detail = data(&c)
            .detail
            .as_deref()
            .expect("the skip's reason is the evidence");
        assert!(detail.contains("cannot read the catalog"), "{detail}");
        assert_eq!(
            data(&c).hint.as_deref(),
            Some("Upgrade bookrack. Then run `bookrack libraries mount beta`.")
        );
    }

    /// An empty registry has one next step, and it is not a name.
    #[test]
    fn registry_empty_hints_at_registering() {
        let c = classified(RegistryError::Empty);
        assert_eq!(c.kind, ErrorKind::InvalidParams);
        let hint = data(&c)
            .hint
            .as_deref()
            .expect("an empty registry has a next step");
        assert!(hint.contains("libraries register"), "{hint}");
    }

    #[test]
    fn config_unknown_library_is_invalid_library() {
        let c = from_config(&ConfigError::UnknownLibrary {
            name: "ghost".into(),
            available: vec!["main".into()],
        });
        assert_eq!(c.kind, ErrorKind::InvalidLibrary);
    }

    /// A command that talks to the embedder folds the failure through
    /// `?` and `.context()`, so the walk has to find the type below the
    /// wraps. An absent model is the operator's own configuration —
    /// the repair is `ollama pull`, and the hint that names it has to
    /// travel with the kind for the kind to be worth anything.
    #[test]
    fn a_bare_model_not_found_is_caller_input_and_keeps_its_hint() {
        let inner: Result<(), EmbedError> = Err(EmbedError::ModelNotFound {
            model: "test-model".into(),
            reason: "model not found, try pulling it first".into(),
        });
        let err: Report = inner
            .context("probe embedding dimension")
            .context("stamps.reconcile")
            .unwrap_err();
        let c = classified(err);
        assert_eq!(c.kind, ErrorKind::InvalidParams);
        assert!(
            data(&c)
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("ollama pull test-model")),
            "the repair must reach the operator"
        );
    }

    /// An unreachable backend is neither caller input nor a bug in this
    /// binary: the same call may succeed once Ollama is up, which is
    /// what `retryable` says and what the dedicated kind carries.
    #[test]
    fn a_bare_unreachable_backend_is_backend_unavailable_and_retryable() {
        let c = classified(EmbedError::Unreachable("connection refused".into()));
        assert_eq!(c.kind, ErrorKind::BackendUnavailable);
        assert!(data(&c).retryable);
        assert!(
            data(&c)
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("BOOKRACK_OLLAMA_URL"))
        );
    }

    /// The same embed failure reached through a pipeline wrapper takes
    /// the same kind as the bare form. Asserted against the literal
    /// kind rather than against the bare form's kind: were the wrappers
    /// not to delegate, both would be the fault kind, and comparing the
    /// two would pass while the classification was wrong.
    #[test]
    fn a_wrapped_model_not_found_takes_the_same_kind_as_a_bare_one() {
        let model_not_found = || EmbedError::ModelNotFound {
            model: "test-model".into(),
            reason: "model not found".into(),
        };
        let wrapped: Vec<Report> = vec![
            IngestError::Embed(model_not_found()).into(),
            GleanError::Embed(model_not_found()).into(),
            OpsError::Query(QueryError::Embed(model_not_found())).into(),
        ];
        for err in wrapped {
            let label = format!("{err:#}");
            assert_eq!(classified(err).kind, ErrorKind::InvalidParams, "{label}");
        }
    }

    /// The request body an embed call sends is assembled by this
    /// binary, so a 4xx that is not an absent model is a fault on this
    /// side. It stays in the residual kind, and the delegate must not
    /// sweep it into the caller-input one.
    #[test]
    fn a_bad_request_from_the_backend_stays_internal() {
        for e in [
            EmbedError::BadRequest {
                status: 400,
                body: "malformed input".into(),
            },
            EmbedError::MalformedResponse("not json".into()),
        ] {
            let label = format!("{e:?}");
            assert_eq!(classified(e).kind, ErrorKind::Internal, "{label}");
        }
    }

    #[test]
    fn user_input_error_message_is_unchanged_by_flattening() {
        let e = OpsError::IntakeNotFound { intake_id: 42 };
        let expected = e.to_string(); // error-boundary-check: allow
        assert_eq!(from_ops(&e).problem.summary, expected);
    }

    /// Pins the recursion itself: a leaf variant two wrappers deep takes
    /// the kind its own classifier assigns. Asserted against the literal
    /// kind — were the wrappers not to delegate, every leaf under them
    /// would be the fault kind, so comparing wrapped to bare would pass
    /// while both were wrong.
    #[test]
    fn a_wrapped_leaf_variant_takes_its_own_kind_through_every_wrapper() {
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
                OpsError::Query(QueryError::Vectors(VectorsError::MissingPqParam(
                    "num_sub_vectors",
                )))
                .into(),
            ),
            (
                "ops > corpus",
                OpsError::Corpus(CorpusError::InvalidIntakeId(-1)).into(),
            ),
            (
                "ingest > corpus",
                IngestError::Corpus(CorpusError::InvalidIntakeId(-1)).into(),
            ),
            (
                "glean > catalog",
                GleanError::Catalog(CatalogError::DerivedFromConflict {
                    intake_id: 7,
                    existing: "aaa".into(),
                    requested: "bbb".into(),
                })
                .into(),
            ),
        ];
        for (label, err) in cases {
            let c = classified(err);
            assert_eq!(
                c.kind,
                ErrorKind::InvalidParams,
                "{label}: {}",
                c.problem.summary
            );
        }
    }

    /// A rule the pipeline refuses is caller input: every such failure
    /// is judged caller input, with the rule's own wording and a next
    /// step rather than the wrapper's name.
    #[test]
    fn a_refused_rule_is_caller_input_with_its_own_wording() {
        use bookrack_extract::rule::RuleError;
        let rule = std::path::PathBuf::from("rule.toml");
        let scan = std::path::PathBuf::from("scan.pdf");
        let cases: Vec<(&str, IngestError)> = vec![
            (
                "rule",
                IngestError::Rule(RuleError::NoMembers { rule: rule.clone() }),
            ),
            ("kind", IngestError::RuleKindNotBook { rule: rule.clone() }),
            (
                "not pending",
                IngestError::RuleSourceNotPending {
                    source_pdf: scan.clone(),
                    status: None,
                },
            ),
            (
                "not a file",
                IngestError::RuleSourceNotAFile {
                    source_pdf: scan.clone(),
                },
            ),
            (
                "unnumbered",
                IngestError::RuleSourceUnnumbered { rule: rule.clone() },
            ),
            (
                "coverage",
                IngestError::RuleCoverage {
                    rule,
                    source_pdf: scan,
                    covered: 2,
                    expected: 3,
                    missing: vec![3],
                    excess: Vec::new(),
                },
            ),
        ];
        for (label, err) in cases {
            let c = classified(Report::from(err).wrap_err("ingest"));
            let summary = &c.problem.summary;
            assert_eq!(c.kind, ErrorKind::InvalidParams, "{label}: {summary}");
            assert!(
                !summary.contains("rule error") && !summary.ends_with('.'),
                "{label}: {summary}"
            );
            assert!(
                data(&c).hint.as_deref().is_some_and(|h| !h.is_empty()),
                "{label}: {c:?}"
            );
        }
    }

    /// A leaf error a command raises without any pipeline wrapper around
    /// it is read by the walk, not left to the caller's residual
    /// channel.
    #[test]
    fn a_bare_leaf_variant_is_recognised_by_the_walk() {
        let inner: Result<(), VectorsError> = Err(VectorsError::MissingPqParam("num_sub_vectors"));
        let err: Report = inner
            .context("build the ANN index")
            .context("vectors.rebuild")
            .unwrap_err();
        assert_eq!(classified(err).kind, ErrorKind::InvalidParams);
        assert_eq!(
            classified(CorpusError::InvalidIntakeId(-1)).kind,
            ErrorKind::InvalidParams
        );
    }

    /// Every variant the classifier judges caller input carries a next
    /// step. The leaf types write no wording of their own, so the hint
    /// is the classifier's, keyed on the kind it chose.
    #[test]
    fn every_caller_input_leaf_variant_carries_a_hint() {
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
                "InvalidAnnConfig",
                VectorsError::InvalidAnnConfig {
                    field: "nprobes",
                    reason: "must be greater than 0, got 0".into(),
                }
                .into(),
            ),
            (
                "TooFewVectors",
                VectorsError::TooFewVectors {
                    kind: bookrack_vectors::AnnKind::IvfFlat,
                    rows: 4,
                    required: 256,
                    limit: TrainingLimit::Partitions {
                        num_partitions: 256,
                    },
                }
                .into(),
            ),
            ("InvalidIntakeId", CorpusError::InvalidIntakeId(0).into()),
            (
                "DerivedFromConflict",
                CatalogError::DerivedFromConflict {
                    intake_id: 7,
                    existing: "aaa".into(),
                    requested: "bbb".into(),
                }
                .into(),
            ),
        ];
        for (label, err) in cases {
            let c = classified(err);
            assert_eq!(c.kind, ErrorKind::InvalidParams, "{label}");
            assert!(
                !data(&c)
                    .hint
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty(),
                "{label}: caller input without a next step"
            );
            assert!(!data(&c).retryable, "{label}");
        }
    }

    /// An embedding backend returning an unusable vector broke its
    /// response contract, so the kind is the fault kind; the wording
    /// is the variant's own, with the way around it as the hint.
    #[test]
    fn an_unusable_embed_vector_is_a_fault_with_a_hint() {
        let c = classified(EmbedError::UnusableVector {
            index: 1,
            defect: bookrack_core::VectorDefect::NonFinite,
        });
        assert_eq!(c.kind, ErrorKind::Internal);
        assert_eq!(
            c.problem.summary,
            "cannot embed: Ollama returned an unusable vector"
        );
        assert!(
            data(&c)
                .hint
                .as_deref()
                .is_some_and(|h| !h.trim().is_empty()),
            "{:?}",
            data(&c).hint
        );
        assert!(!data(&c).retryable);
    }

    /// The hint for a store too small for its index names only routes
    /// that can succeed on the rows it has: no partition count when
    /// there are no rows, and 4-bit codes only where 16 vectors and an
    /// even sub-vector count make them buildable.
    #[test]
    fn too_few_vectors_hints_follow_the_binding_limit() {
        use bookrack_vectors::AnnKind;
        let hint = |rows: u64, required: u64, limit: TrainingLimit| {
            let c = classified(VectorsError::TooFewVectors {
                kind: AnnKind::IvfPq,
                rows,
                required,
                limit,
            });
            assert_eq!(c.kind, ErrorKind::InvalidParams);
            c.problem.data.hint.expect("hint")
        };
        let partitions = TrainingLimit::Partitions {
            num_partitions: 256,
        };
        let codebook = |num_bits, num_sub_vectors| TrainingLimit::PqCodebook {
            num_bits,
            num_sub_vectors,
        };

        let empty = hint(0, 256, partitions);
        assert!(empty.contains("no vectors yet"), "{empty}");
        assert!(!empty.contains("num_partitions"), "{empty}");
        assert!(empty.contains("`bookrack vectors drop`"), "{empty}");

        let small = hint(4, 256, partitions);
        assert!(small.contains("`num_partitions` to at most 4"), "{small}");
        assert!(small.contains("`bookrack papers vectors drop`"), "{small}");

        let even = hint(100, 256, codebook(8, Some(2)));
        assert!(
            even.contains("8-bit PQ codebook needs at least 256"),
            "{even}"
        );
        assert!(even.contains("`num_bits = 4`"), "{even}");
        for (label, rows, limit) in [
            ("odd sub-vectors", 100, codebook(8, Some(3))),
            ("unknown sub-vectors", 100, codebook(8, None)),
            ("fewer than 16 rows", 10, codebook(8, Some(2))),
            ("already 4-bit", 10, codebook(4, Some(2))),
        ] {
            let text = hint(rows, 256, limit);
            assert!(!text.contains("num_bits = 4"), "{label}: {text}");
        }
    }

    #[test]
    fn an_invalid_ann_field_hint_names_the_profile_field_and_the_flag() {
        let c = classified(VectorsError::InvalidAnnConfig {
            field: "refine_factor",
            reason: "must be greater than 0 when set, got 0".into(),
        });
        assert_eq!(c.kind, ErrorKind::InvalidParams);
        assert_eq!(
            c.problem.summary,
            "invalid ANN configuration: refine_factor must be greater than 0 when set, got 0"
        );
        let hint = data(&c).hint.as_deref().expect("hint");
        assert!(hint.contains("`refine_factor`"), "{hint}");
        assert!(hint.contains("`--refine-factor`"), "{hint}");
    }

    /// Delegation must not widen the caller-input kind: a leaf fault
    /// stays the fault kind. The one visible trace of the delegation is
    /// the wrapper's own `Display` ("catalog error: ") leaving the
    /// summary, because the leaf is rendered from its own chain.
    #[test]
    fn a_wrapped_leaf_fault_stays_internal_and_drops_the_wrapper_prefix() {
        let c = classified(OpsError::Catalog(CatalogError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "backup dir read-only",
        ))));
        assert_eq!(c.kind, ErrorKind::Internal);
        assert!(
            !c.problem.summary.starts_with("catalog error"),
            "wrapper Display leaked into the summary: {}",
            c.problem.summary
        );
        assert!(
            c.problem.summary.contains("backup dir read-only"),
            "{}",
            c.problem.summary
        );
    }

    /// Every store variant whose next step is the operator's — a newer
    /// build, a rebuild, a reset — takes the state kind, bare or
    /// wrapped, and names that step. Written out rather than derived
    /// from the `from_*` functions, which are what is under test.
    #[test]
    fn every_unusable_state_variant_takes_the_state_kind_with_a_next_step() {
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
                OpsError::Query(QueryError::Corpus(CorpusError::IndexStampMismatch {
                    key: "embed_model",
                    found: "a".into(),
                    expected: "b".into(),
                }))
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
            let c = classified(err);
            assert_eq!(
                c.kind,
                ErrorKind::StateUnusable,
                "{label}: {}",
                c.problem.summary
            );
            assert!(
                !data(&c)
                    .hint
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty(),
                "{label}: a state kind without the operator's next step"
            );
            assert!(!data(&c).retryable, "{label}");
        }
    }

    /// A catalog behind this build, met through a door that does not
    /// migrate, is a state the operator resolves: the state kind, a
    /// summary naming the store and both versions, and the step.
    #[test]
    fn a_catalog_behind_this_build_takes_the_state_kind_and_names_both_versions() {
        let behind = || CatalogError::SchemaTooOld {
            found: 16,
            expected: 17,
        };
        for (label, err) in [
            ("bare", Report::from(behind())),
            ("ops", OpsError::Catalog(behind()).into()),
        ] {
            let c = classified(err);
            let summary = &c.problem.summary;
            assert_eq!(c.kind, ErrorKind::StateUnusable, "{label}: {summary}");
            assert!(
                summary.starts_with("catalog schema is behind this build")
                    && summary.contains("v16")
                    && summary.contains("v17"),
                "{label}: {summary}"
            );
            assert!(
                data(&c)
                    .hint
                    .as_deref()
                    .unwrap_or_default()
                    .contains("bookrack run"),
                "{label}: the hint names the step that migrates it"
            );
        }
    }

    fn retired_catalog() -> CatalogError {
        CatalogError::RetiredLabels {
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

    /// A catalog holding retired labels, met with no library to name,
    /// is a state the operator resolves: the state kind, a summary
    /// without the counts, a detail that carries each of them, and a
    /// hint that points at the root and names no library.
    #[test]
    fn a_catalog_holding_retired_labels_takes_the_state_kind_in_three_parts() {
        for (label, err) in [
            ("bare", Report::from(retired_catalog())),
            ("ops", OpsError::Catalog(retired_catalog()).into()),
            (
                "wrapped",
                Report::from(retired_catalog()).wrap_err("preflight catalog schema check failed"),
            ),
        ] {
            let c = classified(err);
            assert_eq!(
                c.kind,
                ErrorKind::StateUnusable,
                "{label}: {}",
                c.problem.summary
            );
            assert_eq!(
                c.problem.summary,
                "catalog \"/disk/library/beta/catalog.db\" holds rows from the removed OCR \
                 intake path",
                "{label}"
            );
            let detail = data(&c).detail.as_deref().unwrap_or_default();
            assert!(
                detail.ends_with("format = ocr-markdown (3), derived_from_sha256 (2)."),
                "{label}: {detail}"
            );
            let hint = data(&c).hint.as_deref().unwrap_or_default();
            assert!(hint.contains("'/disk/library/beta'"), "{label}: {hint}");
            assert!(
                hint.contains("`bookrack libraries list`")
                    && hint.contains("`bookrack libraries remove <name> --purge`"),
                "{label}: {hint}"
            );
            assert!(!data(&c).retryable, "{label}");
        }
    }

    /// A reference store this build cannot serve is a state the
    /// operator resolves, wherever on the chain it sits: the state kind,
    /// a summary that names the store from the leaf's own words, and a
    /// next step. The context a caller wrapped it in stays out of the
    /// summary.
    #[test]
    fn a_reference_store_this_build_cannot_serve_takes_the_state_kind_and_names_the_store() {
        let cases: Vec<(&str, RefsError, &str, &[&str])> = vec![
            (
                "SchemaTooNew",
                RefsError::SchemaTooNew {
                    found: 3,
                    supported: 2,
                },
                "reference.db schema is newer than this build supports",
                &["3", "2"],
            ),
            (
                "SchemaTooOld",
                RefsError::SchemaTooOld {
                    found: 1,
                    supported: 2,
                },
                "reference.db schema is behind this build",
                &["1", "2"],
            ),
            (
                "ReaderTooOld",
                RefsError::ReaderTooOld {
                    required: 9,
                    current: 3,
                },
                "reference.db requires a newer reader",
                &["v9", "v3"],
            ),
        ];
        for (label, leaf, opening, numbers) in cases {
            let c = classified(
                Report::from(leaf).wrap_err("preflight reference store schema check failed"),
            );
            let summary = &c.problem.summary;
            assert_eq!(c.kind, ErrorKind::StateUnusable, "{label}: {summary}");
            assert!(summary.starts_with(opening), "{label}: {summary}");
            assert!(
                numbers.iter().all(|n| summary.contains(n)),
                "{label}: both versions are named: {summary}"
            );
            assert!(!summary.contains("preflight"), "{label}: {summary}");
            let hint = data(&c).hint.as_deref().unwrap_or_default();
            assert!(
                hint.ends_with('.') && !hint.contains("libraries mount"),
                "{label}: a full sentence that names the repair and no retry: {hint:?}"
            );
            assert!(!data(&c).retryable, "{label}");
        }
    }

    /// A reference store fault with no next step is read and not
    /// judged: the walk ends there, and the chain is left to the
    /// caller's residual channel.
    #[test]
    fn a_reference_store_fault_with_no_next_step_ends_the_walk_unjudged() {
        let leaf = RefsError::Io(std::io::Error::other("backup directory is not writable"));
        assert_eq!(classify_leaf_cause(&leaf), ControlFlow::Break(None));
        let err = Report::from(leaf).wrap_err("preflight reference store schema check failed");
        assert!(classify_report(&err).is_none());
    }

    /// A cause of no type this module reads lets the walk go on, and a
    /// chain made only of such causes is not judged.
    #[test]
    fn a_chain_of_unknown_causes_is_not_judged() {
        let unknown = std::io::Error::other("disk on fire");
        assert!(classify_ops_cause(&unknown).is_none());
        assert_eq!(classify_leaf_cause(&unknown), ControlFlow::Continue(()));
        let err: Report = eyre::eyre!("disk on fire").wrap_err("rebuild step");
        assert!(classify_report(&err).is_none());
    }

    /// The outermost cause the walk reads decides: a store leaf held in
    /// an untyped ops report is judged by the wrapper, not by the leaf
    /// it arrives bare as.
    #[test]
    fn an_ops_wrapper_decides_over_a_leaf_below_it() {
        let err: Report = OpsError::Other(
            Report::from(CatalogError::SchemaTooNew {
                found: 99,
                expected: 17,
            })
            .wrap_err("open catalog"),
        )
        .into();
        assert!(
            err.chain()
                .any(|cause| cause.downcast_ref::<CatalogError>().is_some()),
            "the leaf is on the chain"
        );
        assert_eq!(classified(err).kind, ErrorKind::Internal);
    }

    /// The lookup's own classifier judges a store stamped for a newer
    /// reader an unusable state and says which reader it needs.
    #[test]
    fn a_lookup_on_a_store_stamped_for_a_newer_reader_takes_the_state_kind() {
        let c = from_reference(&ReferenceError::Open {
            path: std::path::PathBuf::from("/some/where/reference.db"),
            source: RefsError::ReaderTooOld {
                required: 9,
                current: 3,
            },
        });
        assert_eq!(c.kind, ErrorKind::StateUnusable, "{}", c.problem.summary);
        let detail = data(&c).detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("reader version 9") && detail.contains("reader version 3"),
            "the detail names both reader versions: {detail}"
        );
        assert!(
            data(&c).hint.as_deref().unwrap_or_default().contains("v9"),
            "the hint names the reader version to run"
        );
    }

    /// A store behind this build is migrated by a distill build or by
    /// mounting the library; the lookup's hint names both.
    #[test]
    fn a_lookup_on_a_store_behind_this_build_names_both_steps_that_migrate_it() {
        let c = from_reference(&ReferenceError::Open {
            path: std::path::PathBuf::from("/some/where/reference.db"),
            source: RefsError::SchemaTooOld {
                found: 1,
                supported: 2,
            },
        });
        assert_eq!(c.kind, ErrorKind::StateUnusable, "{}", c.problem.summary);
        let hint = data(&c).hint.as_deref().unwrap_or_default();
        assert!(
            hint.contains("bookrack distill build") && hint.contains("bookrack run"),
            "{hint}"
        );
    }

    /// The two read-shape refusals are caller input in the shared
    /// classifier, so the read proxies that route through it do not
    /// demote them to the fault kind.
    #[test]
    fn read_shape_refusals_are_caller_input() {
        for e in [
            OpsError::NotALeaf { node_id: 7 },
            OpsError::NotOrganizing { node_id: 7 },
        ] {
            let label = format!("{e:?}");
            let c = from_ops(&e);
            assert_eq!(c.kind, ErrorKind::InvalidParams, "{label}");
            assert!(
                c.problem.summary.contains("node 7"),
                "{label}: {}",
                c.problem.summary
            );
        }
    }

    /// A passage read of a kind without corpus nodes is caller input,
    /// and its remedy travels in the hint rather than the summary.
    #[test]
    fn a_passage_read_of_a_kind_without_nodes_is_caller_input() {
        let c = from_ops(&OpsError::NoPassagesForKind {
            kind: bookrack_core::ItemKind::Reference,
        });
        assert_eq!(c.kind, ErrorKind::InvalidParams);
        assert_eq!(
            c.problem.summary,
            "cannot read passages of kind \"reference\""
        );
        assert!(
            data(&c)
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("reference.lookup")),
            "{c:?}"
        );
    }

    /// A write session refused because another holds the write mutex
    /// is busy, and it says the same call may succeed once retried.
    #[test]
    fn a_busy_write_session_is_busy_and_retryable() {
        let c = from_ops(&OpsError::WriteBusy(crate::write_session::WriteBusy));
        assert_eq!(c.kind, ErrorKind::Busy);
        assert_eq!(
            c.problem.summary,
            "another write command is already in progress"
        );
        assert!(data(&c).retryable);
    }

    /// A reranker that did not answer, or answered that it is overloaded
    /// or still loading its model, is a backend the same call may reach
    /// later: the backend kind, retryable, with the reranker's own
    /// wording. Asserted against the literal kind, through the wrapper
    /// a search raises it in and under a caller's context.
    #[test]
    fn a_reranker_that_did_not_answer_is_backend_unavailable_and_retryable() {
        use bookrack_rerank::RerankError;
        for e in [
            RerankError::Unreachable("connection refused".into()),
            RerankError::Overloaded {
                status: 503,
                body: "loading model".into(),
            },
        ] {
            let label = format!("{e:?}");
            let c = classified(Report::from(OpsError::Rerank(e)).wrap_err("library.search"));
            assert_eq!(
                c.kind,
                ErrorKind::BackendUnavailable,
                "{label}: {}",
                c.problem.summary
            );
            assert!(data(&c).retryable, "{label}");
            assert!(
                c.problem.summary.contains("reranker"),
                "{label}: {}",
                c.problem.summary
            );
        }
    }

    /// A request the reranker refused and a reply it could not parse
    /// are faults, not a backend that may come back: the backend kind
    /// does not widen to them.
    #[test]
    fn a_refused_or_malformed_rerank_stays_internal() {
        use bookrack_rerank::RerankError;
        for e in [
            RerankError::BadRequest {
                status: 400,
                body: "bad documents".into(),
            },
            RerankError::MalformedResponse("no results".into()),
        ] {
            let label = format!("{e:?}");
            let c = from_ops(&OpsError::Rerank(e));
            assert_eq!(c.kind, ErrorKind::Internal, "{label}");
            assert!(!data(&c).retryable, "{label}");
        }
    }
}
