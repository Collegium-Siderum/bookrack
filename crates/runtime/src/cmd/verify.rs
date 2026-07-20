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

/// Collect verifiable findings for every store under `cfg`. Each
/// database is probed by file presence first and opened through its
/// read-only door only when present, so verify neither materialises a
/// missing store nor takes the write lock a live daemon holds. A data
/// directory with neither `catalog.db` nor `corpus.db` is reported as
/// `not_initialised`; one store present without the other reports the
/// absent one as missing instead of inventing it.
pub fn build_verify_report(cfg: &Config) -> render::VerifyReport {
    let mut report = render::VerifyReport {
        catalog_missing: !cfg.catalog_db().exists(),
        corpus_missing: !cfg.corpus_db().exists(),
        ..Default::default()
    };
    if report.catalog_missing && report.corpus_missing {
        report.not_initialised = true;
        return report;
    }

    // Schema verification happens inside the open paths; surface success
    // as a one-liner per database, and any failure as a multi-line block.
    if !report.catalog_missing {
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
    }
    if !report.corpus_missing {
        match Corpus::open_read_only(&cfg.corpus_db()) {
            Ok(_) => {
                report.corpus_schema_ok = true;
            }
            Err(e) => {
                report.corpus_schema_error = Some(format!("{e:#}"));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn config_for(data_dir: &Path) -> Config {
        Config::new(data_dir.to_path_buf(), "http://localhost:11434".to_string())
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read data dir")
            .map(|e| {
                e.expect("dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_fresh_data_root_reports_not_initialised_and_stays_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let report = build_verify_report(&config_for(dir.path()));
        assert!(report.not_initialised);
        assert!(report.catalog_missing);
        assert!(report.corpus_missing);
        assert!(
            entries(dir.path()).is_empty(),
            "verify must not create files"
        );
    }

    #[test]
    fn a_catalog_only_root_reports_the_corpus_missing_without_creating_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_for(dir.path());
        drop(Catalog::open(&cfg.catalog_db()).expect("create catalog"));
        let before = entries(dir.path());

        let report = build_verify_report(&cfg);
        assert!(!report.not_initialised);
        assert!(report.catalog_schema_ok);
        assert!(report.corpus_missing);
        assert!(!report.corpus_schema_ok);
        assert!(report.corpus_schema_error.is_none());
        assert_eq!(
            entries(dir.path()),
            before,
            "verify must not materialise corpus.db"
        );
    }
}
