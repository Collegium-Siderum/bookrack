// SPDX-License-Identifier: Apache-2.0

//! `bookrack ingest [--recursive]` — drive the EXTRACT → STRUCTURE →
//! CHUNK → EMBED pipeline against one file or every supported file
//! under a directory.

use std::path::{Path, PathBuf};

use bookrack_catalog::Catalog;
use bookrack_config::Config;
use bookrack_corpus::Corpus;
use bookrack_ingest::{IngestParams, ingest_book};
use eyre::{Context, Result};

use crate::audit_helpers::{load_audit_data, load_audit_profile, load_heading_patterns};
use crate::embed_helpers::embedder;
use crate::render;

pub async fn run(
    cfg: &Config,
    path: &Path,
    recursive: bool,
    hold_for_metadata: bool,
    force: bool,
    profile_name: Option<&str>,
) -> Result<()> {
    let embed_cfg = crate::profile::effective_embed_config(cfg)?;
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open corpus")?;
    let mut catalog =
        Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let embedder = embedder(cfg, &embed_cfg)?;
    let audit_data = load_audit_data(cfg);
    let audit_profile = load_audit_profile(cfg, profile_name);
    let heading_patterns = load_heading_patterns(cfg);
    let params = IngestParams {
        embed: embed_cfg,
        hold_for_metadata,
        force,
        audit_data,
        audit_profile,
        heading_patterns,
        ..Default::default()
    };
    let pipeline_run_id = catalog
        .open_pipeline_run("ingest", None, cfg.data_dir().to_str())
        .ok();
    let result = run_inner(
        &mut corpus,
        &mut catalog,
        cfg,
        path,
        recursive,
        force,
        &embedder,
        &params,
    )
    .await;
    if let Some(id) = pipeline_run_id.as_deref() {
        let status = if result.is_ok() { "ok" } else { "error" };
        if let Err(e) = catalog.close_pipeline_run(id, status) {
            tracing::warn!(error = %e, pipeline_run_id = id, "ingest: close_pipeline_run failed");
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_inner<E: bookrack_embed::Embedder>(
    corpus: &mut Corpus,
    catalog: &mut Catalog,
    cfg: &Config,
    path: &Path,
    recursive: bool,
    force: bool,
    embedder: &E,
    params: &IngestParams,
) -> Result<()> {
    if !recursive {
        if path.is_dir() {
            eyre::bail!(
                "{} is a directory; pass --recursive to walk it instead",
                path.display(),
            );
        }
        let report = ingest_book(
            path,
            corpus,
            catalog,
            &cfg.lancedb_dir(),
            &cfg.books_dir(),
            embedder,
            params,
        )
        .await
        .context("ingest book")?;
        render::ingest(&report);
        return Ok(());
    }

    if !path.is_dir() {
        eyre::bail!(
            "--recursive requires a directory; {} is not one",
            path.display()
        );
    }
    let files = crate::queue::collect_supported_files(path)?;
    if files.is_empty() {
        println!("No supported files under {}.", path.display());
        return Ok(());
    }
    println!(
        "Walking {} ({} supported file{}):",
        path.display(),
        files.len(),
        if files.len() == 1 { "" } else { "s" },
    );
    let mut newly_ingested = 0usize;
    let mut refreshed = 0usize;
    let mut skipped_noop = 0usize;
    let mut failed: Vec<(PathBuf, String)> = Vec::new();
    for file in &files {
        match ingest_book(
            file,
            corpus,
            catalog,
            &cfg.lancedb_dir(),
            &cfg.books_dir(),
            embedder,
            params,
        )
        .await
        {
            Ok(report) => {
                let needs_work_tag = if report.audit_verdict.as_deref() == Some("needs_work") {
                    " \u{26a0} needs_work"
                } else {
                    ""
                };
                if report.no_op {
                    skipped_noop += 1;
                    println!(
                        "  = {} (intake {}, already up to date{needs_work_tag})",
                        file.display(),
                        report.intake_id,
                    );
                } else if report.already_registered {
                    refreshed += 1;
                    let marker = if report.forced {
                        "forced"
                    } else {
                        "stamp drift"
                    };
                    println!(
                        "  ~ {} (intake {}, refreshed [{marker}], {} chunks{needs_work_tag})",
                        file.display(),
                        report.intake_id,
                        report.chunks_written,
                    );
                } else {
                    newly_ingested += 1;
                    println!(
                        "  + {} (intake {}, {} chunks{needs_work_tag})",
                        file.display(),
                        report.intake_id,
                        report.chunks_written,
                    );
                }
            }
            Err(e) => {
                let message = format!("{e:#}");
                tracing::warn!(
                    file = %file.display(),
                    error = %message,
                    "ingest failed; continuing",
                );
                println!("  ! {} — failed: {message}", file.display());
                failed.push((file.clone(), message));
            }
        }
    }
    println!();
    println!(
        "Recursive ingest summary: {newly_ingested} new, {refreshed} refreshed, \
         {skipped_noop} already up to date, {} failed.",
        failed.len(),
    );
    if skipped_noop > 0 && !force {
        println!("  (Pass --force to re-extract and re-embed up-to-date intakes.)");
    }
    if !failed.is_empty() {
        eyre::bail!("{} file(s) failed during recursive ingest", failed.len());
    }
    Ok(())
}
