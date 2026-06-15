// SPDX-License-Identifier: Apache-2.0

//! REPL-side metadata edits and the `advance` resume-from-CHUNK path.
//! Reads have moved to `bookrack exec library.show_metadata_audit` and
//! siblings; this module covers only the write surface.

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{Config, EmbedConfig};
use bookrack_core::PartitionIdx;
use bookrack_corpus::Corpus;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ingest::{IngestParams, resume_from_chunk};
use bookrack_ops::Ops;

use crate::audit_helpers::load_audit_profile;
use crate::embed_helpers::embedder;
use crate::ops_helpers::catalog_only_ops;

pub use bookrack_repl_grammar::WriteMetadataAction;

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
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e @ bookrack_ops::OpsError::UnknownMetadataField { .. }) => {
            anyhow::bail!("{e}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("set metadata field via ops")),
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
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e @ bookrack_ops::OpsError::UnknownMetadataField { .. }) => {
            anyhow::bail!("{e}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("clear metadata field via ops")),
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
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e @ bookrack_ops::OpsError::UnknownMetadataField { .. }) => {
            anyhow::bail!("{e}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("void metadata field via ops")),
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
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("reaudit metadata via ops")),
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
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e @ bookrack_ops::OpsError::UnknownContributorRole { .. }) => {
            anyhow::bail!("{e}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("add contributor via ops")),
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
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e @ bookrack_ops::OpsError::ContributorNotFound { .. }) => {
            anyhow::bail!("{e}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("remove contributor via ops")),
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
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("acknowledge metadata gap via ops")),
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
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("approve metadata via ops")),
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
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("reject metadata via ops")),
    }
}

async fn advance(cfg: &Config, book: i64, profile_name: Option<&str>) -> Result<()> {
    let embed_cfg = EmbedConfig::from_env();
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
