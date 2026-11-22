// SPDX-License-Identifier: Apache-2.0

//! REPL-side metadata edits. Reads have moved to
//! `bookrack rpc call library.show_metadata_audit` and its siblings;
//! this module covers only the write surface.

use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::audit_overlay::{load_audit_data, load_audit_profile};
use bookrack_ops::dto::writes::{AddContributorOutcome, ReauditOutcome, WriteOutcome};
use bookrack_ops::write_session::WriteSession;
use eyre::Result;
use serde::Serialize;

/// The write session every metadata write here runs in.
type Session = WriteSession<OllamaEmbedClient>;

pub use bookrack_cli_grammar::WriteMetadataAction;

/// What one write did, in the field names the ops layer and the MCP
/// write tools use. Serializes as a flat object; which fields appear
/// depends on the action.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MetadataWriteReport {
    /// `set`, `clear`, `void`, `ack`, `approve`, `reject`,
    /// `contributor_remove`: the audit identity of the edit, and the
    /// field it touched when the action names one.
    Edit {
        book: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        field: Option<String>,
        #[serde(flatten)]
        outcome: WriteOutcome,
    },
    /// `contributor_add`: the edit's audit identity and the new row.
    ContributorAdded {
        book: i64,
        #[serde(flatten)]
        outcome: AddContributorOutcome,
    },
    /// `reaudit`: the verdict and confidence it stored, beside the
    /// ones it replaced. A re-audit writes no `metadata_audit` row.
    Reaudited {
        book: i64,
        verdict: String,
        previous_verdict: Option<String>,
        confidence: String,
        previous_confidence: Option<String>,
    },
}

/// Dispatch for the write actions, in `session`'s library and
/// attributed to its caller. Opens no store itself: each write takes
/// the catalog's write door inside the ops layer, after its own argument
/// checks. Returns what the write did.
pub fn run_write(
    session: &mut Session,
    action: WriteMetadataAction,
    profile_name: Option<&str>,
) -> Result<MetadataWriteReport> {
    let edit =
        |book: i64, field: Option<String>, outcome: WriteOutcome| MetadataWriteReport::Edit {
            book,
            field,
            outcome,
        };
    let report = match action {
        WriteMetadataAction::Set {
            book,
            field,
            value,
            reason,
            confirmed,
        } => edit(
            book,
            Some(field.clone()),
            set(session, book, &field, &value, reason, confirmed)?,
        ),
        WriteMetadataAction::Clear {
            book,
            field,
            reason,
        } => edit(
            book,
            Some(field.clone()),
            clear(session, book, &field, reason)?,
        ),
        WriteMetadataAction::Void {
            book,
            field,
            reason,
        } => edit(
            book,
            Some(field.clone()),
            void(session, book, &field, reason)?,
        ),
        WriteMetadataAction::Reaudit { book } => {
            let outcome = reaudit(session, book, profile_name)?;
            MetadataWriteReport::Reaudited {
                book,
                verdict: outcome.verdict,
                previous_verdict: outcome.previous_verdict,
                confidence: outcome.confidence,
                previous_confidence: outcome.previous_confidence,
            }
        }
        WriteMetadataAction::ContributorAdd {
            book,
            role,
            name,
            nationality,
            reason,
        } => MetadataWriteReport::ContributorAdded {
            book,
            outcome: contributor_add(session, book, role, name, nationality, reason)?,
        },
        WriteMetadataAction::ContributorRemove {
            book,
            contributor_id,
            reason,
        } => edit(
            book,
            None,
            contributor_remove(session, book, contributor_id, reason)?,
        ),
        WriteMetadataAction::Ack { book, reason } => edit(book, None, ack(session, book, &reason)?),
        WriteMetadataAction::Approve { book, reason } => {
            edit(book, None, approve(session, book, reason.as_deref())?)
        }
        WriteMetadataAction::Reject { book, reason } => {
            edit(book, None, reject(session, book, &reason)?)
        }
    };
    Ok(report)
}

fn set(
    session: &mut Session,
    book: i64,
    field: &str,
    value: &str,
    reason: Option<String>,
    confirmed: bool,
) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::SetMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
        value: value.to_string(),
        reason,
        confirmed,
    };
    match bookrack_ops::writes::metadata::set_metadata_field(session, req) {
        Ok(outcome) => Ok(outcome),
        Err(e @ bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            // The typed error stays in the chain so the control plane
            // maps it to INVALID_PARAMS; the context line carries the
            // operator-facing "book" wording.
            Err(eyre::Report::from(e)
                .wrap_err(format!("no intake registered for book {intake_id}")))
        }
        Err(e @ bookrack_ops::OpsError::UnknownMetadataField { .. }) => Err(e.into()),
        Err(e) => Err(eyre::Report::from(e).wrap_err("set metadata field via ops")),
    }
}

fn clear(
    session: &mut Session,
    book: i64,
    field: &str,
    reason: Option<String>,
) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::ClearMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
        reason,
    };
    match bookrack_ops::writes::metadata::clear_metadata_field(session, req) {
        Ok(outcome) => Ok(outcome),
        Err(e @ bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            // The typed error stays in the chain so the control plane
            // maps it to INVALID_PARAMS; the context line carries the
            // operator-facing "book" wording.
            Err(eyre::Report::from(e)
                .wrap_err(format!("no intake registered for book {intake_id}")))
        }
        Err(e @ bookrack_ops::OpsError::UnknownMetadataField { .. }) => Err(e.into()),
        Err(e) => Err(eyre::Report::from(e).wrap_err("clear metadata field via ops")),
    }
}

fn void(
    session: &mut Session,
    book: i64,
    field: &str,
    reason: Option<String>,
) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::VoidMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
        reason,
    };
    match bookrack_ops::writes::metadata::void_metadata_field(session, req) {
        Ok(outcome) => Ok(outcome),
        Err(e @ bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            // The typed error stays in the chain so the control plane
            // maps it to INVALID_PARAMS; the context line carries the
            // operator-facing "book" wording.
            Err(eyre::Report::from(e)
                .wrap_err(format!("no intake registered for book {intake_id}")))
        }
        Err(e @ bookrack_ops::OpsError::UnknownMetadataField { .. }) => Err(e.into()),
        Err(e) => Err(eyre::Report::from(e).wrap_err("void metadata field via ops")),
    }
}

/// Re-audit `book` under the session library's own overlays: the
/// audit data overlay, and the profile `profile_name` names or the
/// profile overlay when it names none.
fn reaudit(session: &mut Session, book: i64, profile_name: Option<&str>) -> Result<ReauditOutcome> {
    let audit_profile = load_audit_profile(session.handle().cfg(), profile_name);
    let audit_data = load_audit_data(session.handle().cfg());
    let req = bookrack_ops::dto::writes::ReauditMetadataRequest { intake_id: book };
    match bookrack_ops::writes::metadata::reaudit_metadata(
        session,
        req,
        &audit_data,
        &audit_profile,
    ) {
        Ok(outcome) => Ok(outcome),
        Err(e @ bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            // The typed error stays in the chain so the control plane
            // maps it to INVALID_PARAMS; the context line carries the
            // operator-facing "book" wording.
            Err(eyre::Report::from(e)
                .wrap_err(format!("no intake registered for book {intake_id}")))
        }
        Err(e) => Err(eyre::Report::from(e).wrap_err("reaudit metadata via ops")),
    }
}

fn contributor_add(
    session: &mut Session,
    book: i64,
    role: String,
    name: String,
    nationality: Option<String>,
    reason: Option<String>,
) -> Result<AddContributorOutcome> {
    let req = bookrack_ops::dto::writes::AddContributorRequest {
        intake_id: book,
        role,
        name,
        nationality,
        reason,
    };
    match bookrack_ops::writes::metadata::add_contributor(session, req) {
        Ok(outcome) => Ok(outcome),
        Err(e @ bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            // The typed error stays in the chain so the control plane
            // maps it to INVALID_PARAMS; the context line carries the
            // operator-facing "book" wording.
            Err(eyre::Report::from(e)
                .wrap_err(format!("no intake registered for book {intake_id}")))
        }
        // Returned by value rather than formatted: the control plane
        // classifies by downcasting the error chain, and this variant's
        // own Display is already the operator-facing wording.
        Err(e @ bookrack_ops::OpsError::UnknownContributorRole { .. }) => Err(e.into()),
        Err(e) => Err(eyre::Report::from(e).wrap_err("add contributor via ops")),
    }
}

fn contributor_remove(
    session: &mut Session,
    book: i64,
    contributor_id: i64,
    reason: Option<String>,
) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::RemoveContributorRequest {
        intake_id: book,
        contributor_id,
        reason,
    };
    match bookrack_ops::writes::metadata::remove_contributor(session, req) {
        Ok(outcome) => Ok(outcome),
        Err(e @ bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            // The typed error stays in the chain so the control plane
            // maps it to INVALID_PARAMS; the context line carries the
            // operator-facing "book" wording.
            Err(eyre::Report::from(e)
                .wrap_err(format!("no intake registered for book {intake_id}")))
        }
        // Returned by value rather than formatted: the control plane
        // classifies by downcasting the error chain, and this variant's
        // own Display is already the operator-facing wording.
        Err(e @ bookrack_ops::OpsError::ContributorNotFound { .. }) => Err(e.into()),
        Err(e) => Err(eyre::Report::from(e).wrap_err("remove contributor via ops")),
    }
}

fn ack(session: &mut Session, book: i64, reason: &str) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::AcknowledgeMetadataGapRequest {
        intake_id: book,
        reason: reason.to_string(),
    };
    match bookrack_ops::writes::metadata::acknowledge_metadata_gap(session, req) {
        Ok(outcome) => Ok(outcome),
        Err(e @ bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            // The typed error stays in the chain so the control plane
            // maps it to INVALID_PARAMS; the context line carries the
            // operator-facing "book" wording.
            Err(eyre::Report::from(e)
                .wrap_err(format!("no intake registered for book {intake_id}")))
        }
        Err(e) => Err(eyre::Report::from(e).wrap_err("acknowledge metadata gap via ops")),
    }
}

/// Mark the record reviewed and correct. The operator (or an LLM acting
/// on the operator's behalf) is asserting that the effective metadata
/// matches the source; the audit's plausibility verdict is unchanged.
fn approve(session: &mut Session, book: i64, reason: Option<&str>) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::ApproveMetadataRequest {
        intake_id: book,
        reason: reason.map(str::to_string),
    };
    match bookrack_ops::writes::metadata::approve_metadata(session, req) {
        Ok(outcome) => Ok(outcome),
        Err(e @ bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            // The typed error stays in the chain so the control plane
            // maps it to INVALID_PARAMS; the context line carries the
            // operator-facing "book" wording.
            Err(eyre::Report::from(e)
                .wrap_err(format!("no intake registered for book {intake_id}")))
        }
        Err(e) => Err(eyre::Report::from(e).wrap_err("approve metadata via ops")),
    }
}

/// Reject the book. The pipeline rows stay in place so downstream
/// consumers can filter on `rejected`; this records the rejection and
/// the reason in the audit trail.
fn reject(session: &mut Session, book: i64, reason: &str) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::RejectMetadataRequest {
        intake_id: book,
        reason: reason.to_string(),
    };
    match bookrack_ops::writes::metadata::reject_metadata(session, req) {
        Ok(outcome) => Ok(outcome),
        Err(e @ bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            // The typed error stays in the chain so the control plane
            // maps it to INVALID_PARAMS; the context line carries the
            // operator-facing "book" wording.
            Err(eyre::Report::from(e)
                .wrap_err(format!("no intake registered for book {intake_id}")))
        }
        Err(e) => Err(eyre::Report::from(e).wrap_err("reject metadata via ops")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::error_map::handler_err;
    use crate::control::jsonrpc::INVALID_PARAMS;
    use bookrack_catalog::{Catalog, NewIntake};
    use bookrack_config::Config;
    use bookrack_core::ItemKind;
    use bookrack_ops::Caller;
    use bookrack_ops::registry::LibraryRegistry;
    use bookrack_ops::write_session::Region;
    use std::path::Path;

    /// A write session on a catalog-only library over a scratch root,
    /// attributed to the CLI.
    fn session_over(root: &Path) -> Session {
        let handle = crate::control::methods::test_library_handle(root, "default");
        LibraryRegistry::single(std::sync::Arc::clone(&handle))
            .begin_write(&handle, Region::Library, Caller::cli())
            .expect("a fresh registry's mutex is free")
    }

    fn seed_intake(root: &Path, sha: &str) -> i64 {
        Catalog::open_for_write_unbacked(&root.join("catalog.db"))
            .expect("open catalog")
            .register_intake(ItemKind::Book, &NewIntake::new(sha))
            .expect("register intake")
            .into_intake()
            .intake_id
    }

    /// A configuration over a scratch root holding one registered book.
    fn library_with_book() -> (tempfile::TempDir, Config, i64) {
        let dir = tempfile::tempdir().expect("tempdir");
        let book = seed_intake(dir.path(), "sha-report");
        let cfg = Config::new(dir.path().to_path_buf());
        (dir, cfg, book)
    }

    /// Run one write through the cmd entry point and return its report
    /// as the control plane serializes it.
    async fn report(cfg: &Config, action: WriteMetadataAction) -> serde_json::Value {
        let report =
            run_write(&mut session_over(cfg.data_dir()), action, None).expect("the write succeeds");
        serde_json::to_value(report).expect("the report serializes")
    }

    /// The keys of a serialized report, sorted.
    fn keys(value: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("a report is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    /// The `metadata_audit` row a report's `audit_id` names.
    fn audit_row(cfg: &Config, value: &serde_json::Value) -> bookrack_catalog::MetadataAudit {
        let audit_id = value["audit_id"].as_i64().expect("an audit id");
        Catalog::open_for_write_unbacked(&cfg.catalog_db())
            .expect("open catalog")
            .recent_metadata_audit("", 1000)
            .expect("read the audit trail")
            .into_iter()
            .find(|row| row.audit_id == audit_id)
            .unwrap_or_else(|| panic!("no metadata_audit row {audit_id}"))
    }

    const EDIT_KEYS: [&str; 6] = [
        "actor_detail",
        "actor_kind",
        "audit_id",
        "book",
        "changed",
        "field",
    ];
    const REVIEW_KEYS: [&str; 5] = ["actor_detail", "actor_kind", "audit_id", "book", "changed"];

    #[tokio::test]
    async fn a_set_reports_the_field_and_the_audit_row_it_wrote() {
        let (_tmp, cfg, book) = library_with_book();
        let value = report(
            &cfg,
            WriteMetadataAction::Set {
                book,
                field: "title".to_string(),
                value: "A Synthetic Ledger".to_string(),
                reason: None,
                confirmed: false,
            },
        )
        .await;
        assert_eq!(keys(&value), EDIT_KEYS, "{value}");
        assert_eq!(value["book"], book);
        assert_eq!(value["field"], "title");
        assert_eq!(value["changed"], true);
        assert_eq!(audit_row(&cfg, &value).field.as_deref(), Some("title"));
    }

    #[tokio::test]
    async fn a_clear_with_nothing_to_clear_reports_no_change() {
        let (_tmp, cfg, book) = library_with_book();
        let value = report(
            &cfg,
            WriteMetadataAction::Clear {
                book,
                field: "title".to_string(),
                reason: None,
            },
        )
        .await;
        assert_eq!(keys(&value), EDIT_KEYS, "{value}");
        assert_eq!(value["changed"], false, "{value}");
        audit_row(&cfg, &value);
    }

    #[tokio::test]
    async fn a_void_reports_the_field_it_tombstoned() {
        let (_tmp, cfg, book) = library_with_book();
        let value = report(
            &cfg,
            WriteMetadataAction::Void {
                book,
                field: "title".to_string(),
                reason: None,
            },
        )
        .await;
        assert_eq!(keys(&value), EDIT_KEYS, "{value}");
        assert_eq!(value["field"], "title");
        audit_row(&cfg, &value);
    }

    #[tokio::test]
    async fn the_review_writes_report_their_audit_rows() {
        let (_tmp, cfg, book) = library_with_book();
        for action in [
            WriteMetadataAction::Ack {
                book,
                reason: "known gap".to_string(),
            },
            WriteMetadataAction::Reject {
                book,
                reason: "not a book".to_string(),
            },
        ] {
            let value = report(&cfg, action).await;
            assert_eq!(keys(&value), REVIEW_KEYS, "{value}");
            assert_eq!(value["book"], book);
            audit_row(&cfg, &value);
        }
    }

    #[tokio::test]
    async fn a_contributor_add_reports_the_new_row_and_a_remove_its_audit_row() {
        let (_tmp, cfg, book) = library_with_book();
        let added = report(
            &cfg,
            WriteMetadataAction::ContributorAdd {
                book,
                role: "author".to_string(),
                name: "Synthetic Author".to_string(),
                nationality: None,
                reason: None,
            },
        )
        .await;
        let mut expected = REVIEW_KEYS.to_vec();
        expected.push("contributor_id");
        expected.sort_unstable();
        assert_eq!(keys(&added), expected, "{added}");
        audit_row(&cfg, &added);

        let contributor_id = added["contributor_id"].as_i64().expect("a contributor id");
        let removed = report(
            &cfg,
            WriteMetadataAction::ContributorRemove {
                book,
                contributor_id,
                reason: None,
            },
        )
        .await;
        assert_eq!(keys(&removed), REVIEW_KEYS, "{removed}");
        assert_ne!(removed["audit_id"], added["audit_id"]);
        audit_row(&cfg, &removed);
    }

    /// An approval reports the review's audit row, as `ack` and
    /// `reject` do.
    #[tokio::test]
    async fn an_approve_off_the_gate_reports_its_audit_row() {
        let (_tmp, cfg, book) = library_with_book();
        let value = report(&cfg, WriteMetadataAction::Approve { book, reason: None }).await;
        assert_eq!(keys(&value), REVIEW_KEYS, "{value}");
        audit_row(&cfg, &value);
    }

    // The two tests below assert through `handler_err`, the same
    // classifier the control plane dispatches through, because the
    // defect they pin lives between two layers that were each already
    // covered: the ops layer raises the typed variant, and the mapper
    // maps it, but a wrapper here can format the type out of the chain
    // and leave both ends green.

    #[test]
    fn an_unknown_contributor_role_reaches_the_control_plane_as_invalid_params() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut session = session_over(tmp.path());
        let book = seed_intake(tmp.path(), "sha-contributor-role");

        let err = contributor_add(
            &mut session,
            book,
            "narrator".to_string(),
            "Somebody".to_string(),
            None,
            None,
        )
        .expect_err("an unknown contributor role must be refused");

        let rpc = handler_err("metadata.contributor_add", err);
        assert_eq!(
            rpc.code, INVALID_PARAMS,
            "a mistyped role is caller input, not a handler fault; \
             formatting the typed error into the message drops it out of \
             the chain the mapper downcasts"
        );
        assert!(
            rpc.message.contains("narrator"),
            "the offending role survives into the message: {}",
            rpc.message
        );
    }

    #[test]
    fn a_missing_contributor_row_reaches_the_control_plane_as_invalid_params() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut session = session_over(tmp.path());
        let book = seed_intake(tmp.path(), "sha-contributor-row");

        let err = contributor_remove(&mut session, book, 4242, None)
            .expect_err("an unknown contributor id must be refused");

        let rpc = handler_err("metadata.contributor_remove", err);
        assert_eq!(
            rpc.code, INVALID_PARAMS,
            "a contributor id that names no row is caller input"
        );
        assert!(
            rpc.message.contains("4242"),
            "the offending contributor id survives into the message: {}",
            rpc.message
        );
    }

    #[tokio::test]
    async fn a_write_naming_an_unknown_field_on_an_empty_root_creates_no_store() {
        use crate::cmd::test_fixtures::{store_files, temp_cfg};

        let (tmp, cfg) = temp_cfg();

        let err = run_write(
            &mut session_over(cfg.data_dir()),
            WriteMetadataAction::Set {
                book: 1,
                field: "no_such_field".to_string(),
                value: "x".to_string(),
                reason: None,
                confirmed: false,
            },
            None,
        )
        .expect_err("the field is not editable");

        assert!(
            err.chain().any(|cause| matches!(
                cause.downcast_ref::<bookrack_ops::OpsError>(),
                Some(bookrack_ops::OpsError::UnknownMetadataField { .. })
            )),
            "{err:?}",
        );
        assert!(
            store_files(tmp.path()).is_empty(),
            "a refused write created {:?}",
            store_files(tmp.path()),
        );
    }
}
