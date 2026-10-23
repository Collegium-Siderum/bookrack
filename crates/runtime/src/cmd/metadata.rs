// SPDX-License-Identifier: Apache-2.0

//! REPL-side metadata edits and the `advance` resume-from-CHUNK path.
//! Reads have moved to `bookrack rpc call library.show_metadata_audit`
//! and its siblings; this module covers only the write surface.

use bookrack_catalog::Catalog;
use bookrack_config::Config;
use bookrack_core::PartitionIdx;
use bookrack_corpus::Corpus;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ingest::{EmbedRunReport, IngestParams, resume_from_chunk};
use bookrack_ops::Ops;
use bookrack_ops::dto::writes::{AddContributorOutcome, ReauditOutcome, WriteOutcome};
use eyre::{Context, Result};
use serde::Serialize;

use crate::audit_helpers::load_audit_profile;
use crate::cmd::input_error::CmdInputError;
use crate::embed_helpers::embedder;
use crate::ops_helpers::catalog_only_ops;

pub use bookrack_cli_grammar::WriteMetadataAction;

/// What one write did, in the field names the ops layer and the MCP
/// write tools use. Serializes as a flat object; which fields appear
/// depends on the action.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MetadataWriteReport {
    /// `set`, `clear`, `void`, `ack`, `reject`, `contributor_remove`:
    /// the audit identity of the edit, and the field it touched when
    /// the action names one.
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
    /// `advance`: what the resumed CHUNK→EMBED wrote.
    Advanced {
        book: i64,
        /// Always `true`: an advance is a resume.
        resumed: bool,
        #[serde(flatten)]
        resume: ResumeCounts,
    },
    /// `approve`: the edit's audit identity, and — when the book was
    /// parked at the metadata gate and the approval resumed it — what
    /// the resume wrote. `resumed` says which.
    Approved {
        book: i64,
        #[serde(flatten)]
        outcome: WriteOutcome,
        resumed: bool,
        #[serde(flatten)]
        resume: Option<ResumeCounts>,
    },
}

/// What a resumed CHUNK→EMBED wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResumeCounts {
    /// Chunk rows written to the vector store.
    pub chunks_written: usize,
    /// Embed requests issued.
    pub batches: usize,
}

impl From<&EmbedRunReport> for ResumeCounts {
    fn from(report: &EmbedRunReport) -> Self {
        ResumeCounts {
            chunks_written: report.chunks_written,
            batches: report.batches,
        }
    }
}

/// REPL-side dispatch for the write actions. Triggers a pending
/// migration once via `open_with_backup` before each write so the
/// per-call handles inside the ops layer only see the migrated
/// database. Returns what the write did.
pub async fn run_write(
    cfg: &Config,
    action: WriteMetadataAction,
    profile_name: Option<&str>,
) -> Result<MetadataWriteReport> {
    if let WriteMetadataAction::Advance { book } = action {
        let report = advance(cfg, book, profile_name).await?;
        return Ok(MetadataWriteReport::Advanced {
            book,
            resumed: true,
            resume: ResumeCounts::from(&report),
        });
    }
    let _migrate =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let ops = catalog_only_ops(cfg);
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
            set(&ops, book, &field, &value, reason, confirmed)?,
        ),
        WriteMetadataAction::Clear {
            book,
            field,
            reason,
        } => edit(
            book,
            Some(field.clone()),
            clear(&ops, book, &field, reason)?,
        ),
        WriteMetadataAction::Void {
            book,
            field,
            reason,
        } => edit(book, Some(field.clone()), void(&ops, book, &field, reason)?),
        WriteMetadataAction::Reaudit { book } => {
            let outcome = reaudit(cfg, &ops, book, profile_name)?;
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
            outcome: contributor_add(&ops, book, role, name, nationality, reason)?,
        },
        WriteMetadataAction::ContributorRemove {
            book,
            contributor_id,
            reason,
        } => edit(
            book,
            None,
            contributor_remove(&ops, book, contributor_id, reason)?,
        ),
        WriteMetadataAction::Ack { book, reason } => edit(book, None, ack(&ops, book, &reason)?),
        WriteMetadataAction::Approve { book, reason } => {
            let outcome = approve(&ops, book, reason.as_deref())?;
            let resume = if book_is_parked_at_metadata(cfg, book)? {
                Some(ResumeCounts::from(&advance(cfg, book, profile_name).await?))
            } else {
                None
            };
            MetadataWriteReport::Approved {
                book,
                outcome,
                resumed: resume.is_some(),
                resume,
            }
        }
        WriteMetadataAction::Reject { book, reason } => {
            edit(book, None, reject(&ops, book, &reason)?)
        }
        WriteMetadataAction::Advance { .. } => unreachable!("handled above"),
    };
    Ok(report)
}

/// Read the book's pipeline stage and return `true` when the book is
/// parked at the metadata gate (the state `ingest_book` writes when an
/// audit verdict of `needs_work` lands with `hold_for_metadata` set).
fn book_is_parked_at_metadata(cfg: &Config, book: i64) -> Result<bool> {
    use bookrack_corpus::PartitionIdx;
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let book_root_id = PartitionIdx::new(book).root();
    let Some(state) = catalog
        .book_state(book_root_id.get())
        .context("read book state")?
    else {
        return Ok(false);
    };
    Ok(state.current_stage == "metadata")
}

fn set(
    ops: &Ops<OllamaEmbedClient>,
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
    match bookrack_ops::writes::metadata::set_metadata_field(ops, req) {
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
    ops: &Ops<OllamaEmbedClient>,
    book: i64,
    field: &str,
    reason: Option<String>,
) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::ClearMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
        reason,
    };
    match bookrack_ops::writes::metadata::clear_metadata_field(ops, req) {
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
    ops: &Ops<OllamaEmbedClient>,
    book: i64,
    field: &str,
    reason: Option<String>,
) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::VoidMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
        reason,
    };
    match bookrack_ops::writes::metadata::void_metadata_field(ops, req) {
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

fn reaudit(
    cfg: &Config,
    ops: &Ops<OllamaEmbedClient>,
    book: i64,
    profile_name: Option<&str>,
) -> Result<ReauditOutcome> {
    let audit_profile = load_audit_profile(cfg, profile_name);
    let audit_data = bookrack_ingest::AuditData::default_data();
    let req = bookrack_ops::dto::writes::ReauditMetadataRequest { intake_id: book };
    match bookrack_ops::writes::metadata::reaudit_metadata(ops, req, &audit_data, &audit_profile) {
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
    ops: &Ops<OllamaEmbedClient>,
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
    match bookrack_ops::writes::metadata::add_contributor(ops, req) {
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
    ops: &Ops<OllamaEmbedClient>,
    book: i64,
    contributor_id: i64,
    reason: Option<String>,
) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::RemoveContributorRequest {
        intake_id: book,
        contributor_id,
        reason,
    };
    match bookrack_ops::writes::metadata::remove_contributor(ops, req) {
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

fn ack(ops: &Ops<OllamaEmbedClient>, book: i64, reason: &str) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::AcknowledgeMetadataGapRequest {
        intake_id: book,
        reason: reason.to_string(),
    };
    match bookrack_ops::writes::metadata::acknowledge_metadata_gap(ops, req) {
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
fn approve(ops: &Ops<OllamaEmbedClient>, book: i64, reason: Option<&str>) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::ApproveMetadataRequest {
        intake_id: book,
        reason: reason.map(str::to_string),
    };
    match bookrack_ops::writes::metadata::approve_metadata(ops, req) {
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
fn reject(ops: &Ops<OllamaEmbedClient>, book: i64, reason: &str) -> Result<WriteOutcome> {
    let req = bookrack_ops::dto::writes::RejectMetadataRequest {
        intake_id: book,
        reason: reason.to_string(),
    };
    match bookrack_ops::writes::metadata::reject_metadata(ops, req) {
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

async fn advance(cfg: &Config, book: i64, profile_name: Option<&str>) -> Result<EmbedRunReport> {
    let embed_cfg = crate::profile::effective_embed_config(cfg)?;
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let audit_profile = load_audit_profile(cfg, profile_name);

    let book_root_id = PartitionIdx::new(book).root();
    let intake = catalog
        .intake_by_id(book)
        .context("look up intake")?
        .ok_or(CmdInputError::UnknownIntake { intake_id: book })?;
    let state = catalog
        .book_state(book_root_id.get())
        .context("read book state")?
        .ok_or_else(|| CmdInputError::Refused {
            summary: format!("no book state for book {book}"),
            hint: Some("Ingest the book before advancing it.".to_string()),
        })?;
    let parsed_at = state
        .parsed_at
        .clone()
        .ok_or_else(|| CmdInputError::Refused {
            summary: format!("book {book} has no parsed_at"),
            hint: Some("Run the STRUCTURE pass before advancing it.".to_string()),
        })?;
    // Mint a fresh run id so resume rows are distinguishable from the
    // original ingest's; pin them to the same source_sha for traceability.
    let run_id = format!(
        "advance-{}-{book}",
        &intake.source_sha256[..8.min(intake.source_sha256.len())]
    );
    let params = IngestParams {
        embed: embed_cfg,
        audit_profile,
        ..Default::default()
    };
    let embedder = embedder(cfg, &params.embed)?;

    let report = resume_from_chunk(
        &mut corpus,
        &mut catalog,
        &cfg.lancedb_dir(),
        &embedder,
        &params,
        book,
        book_root_id,
        &run_id,
        &intake.source_sha256,
        &parsed_at,
        &|_, _| {},
    )
    .await
    .context("resume CHUNK→EMBED")?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::error_map::handler_err;
    use crate::control::jsonrpc::INVALID_PARAMS;
    use bookrack_catalog::NewIntake;
    use bookrack_core::ItemKind;
    use bookrack_ops::Caller;
    use std::path::Path;

    /// A catalog-only `Ops` over a scratch root, matching what
    /// [`catalog_only_ops`] builds from a `Config`.
    fn ops_over(root: &Path) -> Ops<OllamaEmbedClient> {
        Ops::catalog_only(
            root.join("corpus.db"),
            root.join("catalog.db"),
            &root.join("lancedb"),
            root.join("books"),
            root.join("backup"),
            Caller::cli(),
        )
    }

    fn seed_intake(root: &Path, sha: &str) -> i64 {
        Catalog::open(&root.join("catalog.db"))
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
        let cfg = Config::new(
            dir.path().to_path_buf(),
            "http://127.0.0.1:11434".to_string(),
        );
        (dir, cfg, book)
    }

    /// Run one write through the cmd entry point and return its report
    /// as the control plane serializes it.
    async fn report(cfg: &Config, action: WriteMetadataAction) -> serde_json::Value {
        let report = run_write(cfg, action, None)
            .await
            .expect("the write succeeds");
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
        Catalog::open(&cfg.catalog_db())
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

    /// A book not parked at the metadata gate is only approved: the
    /// report says it did not resume and carries no resume counts.
    #[tokio::test]
    async fn an_approve_off_the_gate_reports_no_resume() {
        let (_tmp, cfg, book) = library_with_book();
        let value = report(&cfg, WriteMetadataAction::Approve { book, reason: None }).await;
        let mut expected = REVIEW_KEYS.to_vec();
        expected.push("resumed");
        expected.sort_unstable();
        assert_eq!(keys(&value), expected, "{value}");
        assert_eq!(value["resumed"], false);
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
        let ops = ops_over(tmp.path());
        let book = seed_intake(tmp.path(), "sha-contributor-role");

        let err = contributor_add(
            &ops,
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
        let ops = ops_over(tmp.path());
        let book = seed_intake(tmp.path(), "sha-contributor-row");

        let err = contributor_remove(&ops, book, 4242, None)
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
}
