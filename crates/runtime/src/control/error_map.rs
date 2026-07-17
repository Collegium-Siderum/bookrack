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
//! [`RegistryError`]) and, when one matches, maps a user-input
//! variant onto [`INVALID_PARAMS`] (or the bookrack-specific code
//! reserved for that shape, e.g. [`INVALID_LIBRARY`]). Anything that
//! does not match a known user-input variant falls through to
//! [`INTERNAL_ERROR`].

use bookrack_config::ConfigError;
use bookrack_glean::GleanError;
use bookrack_ingest::IngestError;
use bookrack_ops::OpsError;
use bookrack_ops::registry::RegistryError;
use eyre::Report;

use super::jsonrpc::{
    INTERNAL_ERROR, INVALID_LIBRARY, INVALID_PARAMS, PLAN_KIND_MISMATCH, PLAN_LIBRARY_MISMATCH,
    PLAN_NOT_FOUND, RpcError,
};
use super::plan_registry::PlanLookupError;

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

fn from_ops(e: &OpsError) -> RpcError {
    use OpsError::*;
    match e {
        IntakeNotFound { .. }
        | UnknownMetadataField { .. }
        | UnknownContributorRole { .. }
        | ContributorNotFound { .. }
        | NodeNotFound { .. }
        | NotALeaf { .. }
        | NotOrganizing { .. }
        | SourceNotArchived { .. } => RpcError::new(INVALID_PARAMS, e.to_string()),
        _ => RpcError::new(INTERNAL_ERROR, e.to_string()),
    }
}

fn from_ingest(e: &IngestError) -> RpcError {
    use IngestError::*;
    match e {
        EmptyExtraction
        | NeedsOcr { .. }
        | UnknownIntake(_)
        | MissingEnvelope(_)
        | EnvelopeMismatch(_)
        | IntakeNotEmbedded(_)
        | OcrSourceStatusMismatch { .. }
        | OcrPagesMissing { .. }
        | OcrPagesExcess { .. } => RpcError::new(INVALID_PARAMS, e.to_string()),
        _ => RpcError::new(INTERNAL_ERROR, e.to_string()),
    }
}

fn from_glean(e: &GleanError) -> RpcError {
    use GleanError::*;
    match e {
        NeedsOcr { .. }
        | UnknownIntake(_)
        | IntakeNotRebuildable(_)
        | MissingEnvelope(_)
        | EnvelopeMismatch(_) => RpcError::new(INVALID_PARAMS, e.to_string()),
        _ => RpcError::new(INTERNAL_ERROR, e.to_string()),
    }
}

fn from_registry(e: &RegistryError) -> RpcError {
    match e {
        RegistryError::LibraryUnknown { .. } => RpcError::new(INVALID_LIBRARY, e.to_string()),
        RegistryError::Empty => RpcError::new(INVALID_PARAMS, e.to_string()),
        _ => RpcError::new(INTERNAL_ERROR, e.to_string()),
    }
}

fn from_config(e: &ConfigError) -> RpcError {
    match e {
        ConfigError::UnknownLibrary { .. } => RpcError::new(INVALID_LIBRARY, e.to_string()),
        _ => RpcError::new(INTERNAL_ERROR, e.to_string()),
    }
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

    #[test]
    fn unknown_error_falls_through_to_internal() {
        let err: Report = eyre::eyre!("disk on fire");
        let rpc = write_err("vectors.rebuild", err);
        assert_eq!(rpc.code, INTERNAL_ERROR);
        assert!(rpc.message.contains("vectors.rebuild"));
        assert!(rpc.message.contains("disk on fire"));
    }
}
