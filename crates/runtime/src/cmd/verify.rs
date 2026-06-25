// SPDX-License-Identifier: Apache-2.0

//! `bookrack verify` — per-store schema and on-disk file checks.

use bookrack_catalog::{Catalog, IntakeFilter};
use bookrack_config::Config;
use bookrack_corpus::Corpus;
use eyre::{Context, Result};

use crate::render;

pub fn run(cfg: &Config) -> Result<()> {
    let report = build_verify_report(cfg);
    render::verify(&report);
    if report.catalog_schema_error.is_some() || report.corpus_schema_error.is_some() {
        eyre::bail!("one or more stores failed verification");
    }
    Ok(())
}

/// Collect verifiable findings for every store under `cfg`. A data
/// directory whose `catalog.db` does not yet exist is reported as
/// `not_initialised` and no stores are opened, so verify stays
/// side-effect-free on a freshly created directory.
pub fn build_verify_report(cfg: &Config) -> render::VerifyReport {
    let mut report = render::VerifyReport::default();

    if !cfg.catalog_db().exists() {
        report.not_initialised = true;
        return report;
    }

    // Schema verification happens inside the open paths; surface success
    // as a one-liner per database, and any failure as a multi-line block.
    match Catalog::open_read_only(&cfg.catalog_db()) {
        Ok(catalog) => {
            report.catalog_schema_ok = true;
            report.intake_count = catalog.count_intakes().ok();
            report.missing_intake_files = scan_intake_files(cfg, &catalog).ok();
        }
        Err(e) => {
            report.catalog_schema_error = Some(format!("{e:#}"));
        }
    }
    match Corpus::open(&cfg.corpus_db()) {
        Ok(_) => {
            report.corpus_schema_ok = true;
        }
        Err(e) => {
            report.corpus_schema_error = Some(format!("{e:#}"));
        }
    }
    let vectors_meta = bookrack_vectors::meta::load(&cfg.lancedb_dir())
        .ok()
        .flatten();
    if let Some(meta) = &vectors_meta {
        report.vectors_built_at_chunk_count = Some(meta.built_at_chunk_count);
        report.vectors_churn = Some(meta.churn_since_rebuild);
    }
    report
}

/// Walk every intake row, resolve its `stored_path` under `books/`, and
/// return the intake ids whose file is missing. `None` is returned only
/// when the catalog could not be enumerated.
fn scan_intake_files(cfg: &Config, catalog: &Catalog) -> Result<Vec<i64>> {
    let intakes = catalog
        .find_intakes(&IntakeFilter::default(), u32::MAX, 0)
        .context("enumerate intakes")?;
    let books_root = cfg.books_dir();
    let mut missing = Vec::new();
    for intake in intakes {
        let Some(stored) = intake.stored_path else {
            continue;
        };
        let resolved = books_root.join(&stored);
        if !resolved.exists() {
            missing.push(intake.intake_id);
        }
    }
    Ok(missing)
}
