// SPDX-License-Identifier: Apache-2.0

//! `bookrack metadata` — metadata reads, edits, audits, and the
//! `advance` resume-from-CHUNK path. Also hosts the `audit-profile`
//! reflection commands that need no catalog.

use anyhow::{Context, Result};
use bookrack_catalog::{Catalog, IntakeFilter};
use bookrack_config::{Config, EmbedConfig};
use bookrack_core::PartitionIdx;
use bookrack_corpus::Corpus;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ingest::{IngestParams, resume_from_chunk};
use bookrack_ops::Ops;

use crate::audit_helpers::load_audit_profile;
use crate::embed_helpers::embedder;
use crate::ops_helpers::catalog_only_ops;
use crate::render;
use crate::{AuditProfileAction, MetadataAction, WriteMetadataAction};

/// Logical address of the book root; the CLI's metadata commands only
/// touch this scope, matching the audit and the ingest sub-step.
const BOOK_SCOPE: &str = "book";

pub async fn run(cfg: &Config, action: MetadataAction, _profile_name: Option<&str>) -> Result<()> {
    // The audit-profile reflection commands need no catalog and no audit
    // rules, so they short-circuit before the catalog open.
    if let MetadataAction::AuditProfile { action } = action {
        return audit_profile(action);
    }
    // Trigger any pending catalog migration (with a pre-migration
    // backup snapshot) once before dispatching.
    let catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let ops = catalog_only_ops(cfg);
    match action {
        MetadataAction::Show { book, json } => show(&ops, book, json),
        MetadataAction::List {
            needs_review,
            limit,
            offset,
            json,
        } => list(&ops, &catalog, needs_review, limit, offset, json),
        MetadataAction::AuditTrail { book, json } => audit_trail(&ops, book, json),
        MetadataAction::AuditProfile { .. } => unreachable!("handled above"),
    }
}

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
    let _migrate =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let ops = catalog_only_ops(cfg);
    match action {
        WriteMetadataAction::Set { book, field, value } => set(&ops, book, &field, &value),
        WriteMetadataAction::Clear { book, field } => clear(&ops, book, &field),
        WriteMetadataAction::Ack { book, reason } => ack(&ops, book, &reason),
        WriteMetadataAction::Approve { book, reason } => approve(&ops, book, reason.as_deref()),
        WriteMetadataAction::Reject { book, reason } => reject(&ops, book, &reason),
        WriteMetadataAction::Advance { .. } => unreachable!("handled above"),
    }
}

fn audit_profile(action: AuditProfileAction) -> Result<()> {
    match action {
        AuditProfileAction::List { json } => {
            if json {
                render::audit_profile_names_json(bookrack_audit_profile::ALL_BUILT_IN_NAMES);
            } else {
                for name in bookrack_audit_profile::ALL_BUILT_IN_NAMES {
                    println!("{name}");
                }
            }
            Ok(())
        }
        AuditProfileAction::Show { name } => {
            let profile = bookrack_audit_profile::AuditProfile::from_named(&name)
                .with_context(|| format!("unknown profile {name:?}"))?;
            render::audit_profile_show(&name, &profile);
            Ok(())
        }
        AuditProfileAction::Diff { a, b } => {
            let pa = bookrack_audit_profile::AuditProfile::from_named(&a)
                .with_context(|| format!("unknown profile {a:?}"))?;
            let pb = bookrack_audit_profile::AuditProfile::from_named(&b)
                .with_context(|| format!("unknown profile {b:?}"))?;
            render::audit_profile_diff(&a, &pa, &b, &pb);
            Ok(())
        }
    }
}

fn list(
    ops: &Ops<OllamaEmbedClient>,
    catalog: &Catalog,
    needs_review: bool,
    limit: u32,
    offset: u32,
    json: bool,
) -> Result<()> {
    if needs_review {
        let page = bookrack_ops::reads::metadata::list_pending_reviews(ops, limit, offset)
            .context("list pending reviews")?;
        let rows: Vec<render::MetadataListRow> = page
            .rows
            .into_iter()
            .map(|r| render::MetadataListRow {
                intake_id: r.intake_id,
                title: r.title,
                confidence: r.confidence,
                review_status: r.review_status,
            })
            .collect();
        if json {
            render::metadata_list_json(&rows, page.total);
        } else {
            render::metadata_list(&rows, page.total, true);
        }
        return Ok(());
    }
    // The unfiltered listing has no ops wrapper yet; it stays direct
    // and is not recorded in `mcp_tool_calls`. Switching it through
    // ops needs a new `list_books_for_review` read that surfaces
    // confidence and review status, which is out of scope here.
    let filter = IntakeFilter::default();
    let intakes = catalog
        .find_intakes(&filter, limit, offset)
        .context("find intakes")?;
    let total = catalog
        .count_find_intakes(&filter)
        .context("count intakes")?;
    let mut rows = Vec::with_capacity(intakes.len());
    for intake in intakes {
        let effective = catalog
            .effective_publication_attrs(intake.intake_id, BOOK_SCOPE)
            .context("read effective metadata")?;
        let title = effective.get("title").map(str::to_string);
        let attrs = catalog
            .publication_attrs(intake.intake_id, BOOK_SCOPE)
            .context("read publication attrs")?;
        let confidence = attrs.as_ref().and_then(|a| a.confidence.clone());
        let review = catalog
            .review(intake.intake_id, BOOK_SCOPE)
            .context("read review")?
            .map(|r| r.status);
        rows.push(render::MetadataListRow {
            intake_id: intake.intake_id,
            title,
            confidence,
            review_status: review,
        });
    }
    if json {
        render::metadata_list_json(&rows, total);
    } else {
        render::metadata_list(&rows, total, needs_review);
    }
    Ok(())
}

fn audit_trail(ops: &Ops<OllamaEmbedClient>, book: i64, json: bool) -> Result<()> {
    let rows = bookrack_ops::reads::metadata::show_audit_trail(ops, book)
        .context("read metadata audit")?;
    if json {
        render::metadata_audit_trail_json(book, &rows);
    } else {
        render::metadata_audit_trail(book, &rows);
    }
    Ok(())
}

fn show(ops: &Ops<OllamaEmbedClient>, book: i64, json: bool) -> Result<()> {
    let report = bookrack_ops::reads::metadata::show_metadata_audit(ops, book)
        .context("read metadata audit")?;
    if json {
        render::metadata_show_json(&report);
    } else {
        render::metadata_show(&report);
    }
    Ok(())
}

fn set(ops: &Ops<OllamaEmbedClient>, book: i64, field: &str, value: &str) -> Result<()> {
    let req = bookrack_ops::dto::writes::SetMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
        value: value.to_string(),
    };
    match bookrack_ops::writes::metadata::set_metadata_field(ops, req) {
        Ok(_) => {
            println!("Set {field} on book {book}.");
            Ok(())
        }
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => Err(anyhow::Error::from(e).context("set metadata field via ops")),
    }
}

fn clear(ops: &Ops<OllamaEmbedClient>, book: i64, field: &str) -> Result<()> {
    let req = bookrack_ops::dto::writes::ClearMetadataFieldRequest {
        intake_id: book,
        field: field.to_string(),
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
        Err(e) => Err(anyhow::Error::from(e).context("clear metadata field via ops")),
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
