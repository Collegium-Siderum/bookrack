// SPDX-License-Identifier: Apache-2.0

//! REPL-side metadata edits and the `advance` resume-from-CHUNK path.
//! Reads have moved to `bookrack exec library.show_metadata_audit` and
//! siblings; this module covers only the write surface.

use bookrack_catalog::Catalog;
use bookrack_config::Config;
use bookrack_core::PartitionIdx;
use bookrack_corpus::Corpus;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ingest::{IngestParams, resume_from_chunk};
use bookrack_ops::Ops;
use eyre::{Context, ContextCompat, Result};

use crate::audit_helpers::load_audit_profile;
use crate::embed_helpers::embedder;
use crate::ops_helpers::catalog_only_ops;

pub use bookrack_cli_grammar::WriteMetadataAction;

/// REPL-side dispatch for the write actions. Triggers a pending
/// migration once via `open_with_backup` before each write so the
/// per-call handles inside the ops layer only see the migrated
/// database.
pub async fn run_write(
    cfg: &Config,
    action: WriteMetadataAction,
    profile_name: Option<&str>,
) -> Result<()> {
    if let WriteMetadataAction::Advance { book } = action {
        return advance(cfg, book, profile_name).await;
    }
    let approve_book = if let WriteMetadataAction::Approve { book, .. } = &action {
        Some(*book)
    } else {
        None
    };
    let _migrate =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let ops = catalog_only_ops(cfg);
    match action {
        WriteMetadataAction::Set {
            book,
            field,
            value,
            reason,
            confirmed,
        } => set(&ops, book, &field, &value, reason, confirmed)?,
        WriteMetadataAction::Clear {
            book,
            field,
            reason,
        } => clear(&ops, book, &field, reason)?,
        WriteMetadataAction::Void {
            book,
            field,
            reason,
        } => void(&ops, book, &field, reason)?,
        WriteMetadataAction::Reaudit { book } => reaudit(cfg, &ops, book, profile_name)?,
        WriteMetadataAction::ContributorAdd {
            book,
            role,
            name,
            nationality,
            reason,
        } => contributor_add(&ops, book, role, name, nationality, reason)?,
        WriteMetadataAction::ContributorRemove {
            book,
            contributor_id,
            reason,
        } => contributor_remove(&ops, book, contributor_id, reason)?,
        WriteMetadataAction::Ack { book, reason } => ack(&ops, book, &reason)?,
        WriteMetadataAction::Approve { book, reason } => approve(&ops, book, reason.as_deref())?,
        WriteMetadataAction::Reject { book, reason } => reject(&ops, book, &reason)?,
        WriteMetadataAction::Advance { .. } => unreachable!("handled above"),
    }
    if let Some(book) = approve_book
        && book_is_parked_at_metadata(cfg, book)?
    {
        advance(cfg, book, profile_name).await?;
    }
    Ok(())
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
) -> Result<()> {
    let req = bookrack_ops::dto::writes::SetMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
        value: value.to_string(),
        reason,
        confirmed,
    };
    match bookrack_ops::writes::metadata::set_metadata_field(ops, req) {
        Ok(_) => {
            println!("Set {field} on book {book}.");
            Ok(())
        }
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
) -> Result<()> {
    let req = bookrack_ops::dto::writes::ClearMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
        reason,
    };
    match bookrack_ops::writes::metadata::clear_metadata_field(ops, req) {
        Ok(outcome) => {
            if outcome.changed {
                println!("Cleared override on {field} for book {book}.");
            } else {
                println!("No override on {field} for book {book}; nothing to clear.");
            }
            Ok(())
        }
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
) -> Result<()> {
    let req = bookrack_ops::dto::writes::VoidMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
        reason,
    };
    match bookrack_ops::writes::metadata::void_metadata_field(ops, req) {
        Ok(outcome) => {
            if outcome.changed {
                println!("Voided {field} on book {book}; the field now reads as absent.");
            } else {
                println!(
                    "Voided {field} on book {book}; it had no effective value, the tombstone is recorded."
                );
            }
            Ok(())
        }
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
) -> Result<()> {
    let audit_profile = load_audit_profile(cfg, profile_name);
    let audit_data = bookrack_ingest::AuditData::default_data();
    let req = bookrack_ops::dto::writes::ReauditMetadataRequest { intake_id: book };
    match bookrack_ops::writes::metadata::reaudit_metadata(ops, req, &audit_data, &audit_profile) {
        Ok(outcome) => {
            println!(
                "Reaudited book {book}: verdict {} (was {}), confidence {} (was {}).",
                outcome.verdict,
                outcome.previous_verdict.as_deref().unwrap_or("unset"),
                outcome.confidence,
                outcome.previous_confidence.as_deref().unwrap_or("unset"),
            );
            Ok(())
        }
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
) -> Result<()> {
    let req = bookrack_ops::dto::writes::AddContributorRequest {
        intake_id: book,
        role: role.clone(),
        name: name.clone(),
        nationality,
        reason,
    };
    match bookrack_ops::writes::metadata::add_contributor(ops, req) {
        Ok(outcome) => {
            println!(
                "Added {role} {name:?} to book {book} (contributor id {}).",
                outcome.contributor_id
            );
            Ok(())
        }
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
) -> Result<()> {
    let req = bookrack_ops::dto::writes::RemoveContributorRequest {
        intake_id: book,
        contributor_id,
        reason,
    };
    match bookrack_ops::writes::metadata::remove_contributor(ops, req) {
        Ok(_) => {
            println!("Removed contributor row {contributor_id} from book {book}.");
            Ok(())
        }
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

fn ack(ops: &Ops<OllamaEmbedClient>, book: i64, reason: &str) -> Result<()> {
    let req = bookrack_ops::dto::writes::AcknowledgeMetadataGapRequest {
        intake_id: book,
        reason: reason.to_string(),
    };
    match bookrack_ops::writes::metadata::acknowledge_metadata_gap(ops, req) {
        Ok(_) => {
            println!("Acknowledged metadata gap on book {book}.");
            Ok(())
        }
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
fn approve(ops: &Ops<OllamaEmbedClient>, book: i64, reason: Option<&str>) -> Result<()> {
    let req = bookrack_ops::dto::writes::ApproveMetadataRequest {
        intake_id: book,
        reason: reason.map(str::to_string),
    };
    match bookrack_ops::writes::metadata::approve_metadata(ops, req) {
        Ok(_) => {
            println!("Approved metadata on book {book}.");
            Ok(())
        }
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
fn reject(ops: &Ops<OllamaEmbedClient>, book: i64, reason: &str) -> Result<()> {
    let req = bookrack_ops::dto::writes::RejectMetadataRequest {
        intake_id: book,
        reason: reason.to_string(),
    };
    match bookrack_ops::writes::metadata::reject_metadata(ops, req) {
        Ok(_) => {
            println!("Rejected book {book}.");
            Ok(())
        }
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

async fn advance(cfg: &Config, book: i64, profile_name: Option<&str>) -> Result<()> {
    let embed_cfg = crate::profile::effective_embed_config(cfg)?;
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let audit_profile = load_audit_profile(cfg, profile_name);

    let book_root_id = PartitionIdx::new(book).root();
    let intake = catalog
        .intake_by_id(book)
        .context("look up intake")?
        .with_context(|| format!("no intake registered for book {book}"))?;
    let state = catalog
        .book_state(book_root_id.get())
        .context("read book state")?
        .with_context(|| format!("no book state for book {book}"))?;
    let parsed_at = state
        .parsed_at
        .clone()
        .with_context(|| format!("book {book} has no parsed_at; STRUCTURE has not run"))?;
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
    )
    .await
    .context("resume CHUNK→EMBED")?;
    println!(
        "Advanced book {book}: embedded {} chunks across {} batches.",
        report.chunks_written, report.batches
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::error_map::write_err;
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

    // The two tests below assert through `write_err`, the same
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

        let rpc = write_err("metadata.contributor_add", err);
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

        let rpc = write_err("metadata.contributor_remove", err);
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
