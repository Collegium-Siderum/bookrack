// SPDX-License-Identifier: Apache-2.0

//! `bookrack verify` — per-store schema and on-disk file checks. The
//! report is built here and serialised by the `verify.run` control-plane
//! method; nothing in this module prints.

use bookrack_catalog::{Catalog, IntakeFilter};
use bookrack_config::Config;
use bookrack_corpus::Corpus;
use eyre::{Context, Result};

/// Per-store findings the `verify.run` control-plane method returns.
/// Every field is optional: an unverifiable store leaves its schema flag
/// false and its error populated, and the rest skip the counts that
/// depend on it.
#[derive(Default, serde::Serialize)]
pub struct VerifyReport {
    /// Set when the data directory has neither `catalog.db` nor
    /// `corpus.db` — verify short-circuits in that case and reports
    /// nothing else.
    pub not_initialised: bool,
    /// Set when `catalog.db` is absent while `corpus.db` exists; the
    /// store is reported missing rather than opened into existence.
    pub catalog_missing: bool,
    /// Set when `corpus.db` is absent while `catalog.db` exists.
    pub corpus_missing: bool,
    pub catalog_schema_ok: bool,
    pub catalog_schema_error: Option<String>,
    pub corpus_schema_ok: bool,
    pub corpus_schema_error: Option<String>,
    pub intake_count: Option<u64>,
    pub missing_intake_files: Option<Vec<i64>>,
    /// Why the intake rows could not be read, flattened to its full
    /// source chain. Set when `catalog.db` opened and its schema
    /// verified but a read of the table failed, which leaves the two
    /// counts above absent. Distinct from `intake_count` being
    /// `Some(0)`, which is a readable catalog holding no intakes.
    pub intake_scan_error: Option<String>,
    pub vectors_built_at_chunk_count: Option<u64>,
    pub vectors_churn: Option<u64>,
    /// Why `vectors_meta.json` could not be read, flattened to its full
    /// source chain. Distinct from all three counts being absent, which
    /// is a library that never built an ANN index.
    pub vectors_meta_error: Option<String>,
}

/// Collect verifiable findings for every store under `cfg`. Each
/// database is probed by file presence first and opened through its
/// read-only door only when present, so verify neither materialises a
/// missing store nor takes the write lock a live daemon holds. A data
/// directory with neither `catalog.db` nor `corpus.db` is reported as
/// `not_initialised`; one store present without the other reports the
/// absent one as missing instead of inventing it.
pub fn build_verify_report(cfg: &Config) -> VerifyReport {
    let mut report = VerifyReport {
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
                // A store whose schema verifies can still fail to be
                // read. Both probes go through the same connection, so
                // the first reason is kept and the second is dropped
                // rather than overwriting it.
                match catalog.count_intakes() {
                    Ok(count) => report.intake_count = Some(count),
                    // The variants are wrappers, so the chain is
                    // flattened before it crosses the RPC boundary.
                    Err(e) => report.intake_scan_error = Some(bookrack_core::error_chain(&e)),
                }
                match scan_intake_files(cfg, &catalog) {
                    Ok(missing) => report.missing_intake_files = Some(missing),
                    Err(e) => {
                        report.intake_scan_error.get_or_insert(format!("{e:#}"));
                    }
                }
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
    // `load` already separates an absent sidecar (`Ok(None)`, a library
    // that never built an index) from an unreadable one. Keep the two
    // apart in the report: the error variants are wrappers, so the
    // chain is flattened before it crosses the RPC boundary.
    let vectors_meta = match bookrack_vectors::meta::load(&cfg.lancedb_dir()) {
        Ok(meta) => meta,
        Err(e) => {
            report.vectors_meta_error = Some(bookrack_core::error_chain(&e));
            None
        }
    };
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

    /// Database files under `dir`. Sidecar `-shm` / `-wal` files are
    /// excluded: a read-only connection to an existing WAL database
    /// may create them, so the no-materialisation contract is about
    /// `.db` files, not about the directory being byte-identical.
    fn db_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read data dir")
            .map(|e| {
                e.expect("dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.ends_with(".db"))
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
            std::fs::read_dir(dir.path())
                .expect("read data dir")
                .next()
                .is_none(),
            "verify must not create files"
        );
    }

    /// A catalog-only data root under `dir`, so the report gets past
    /// the `not_initialised` short circuit and reaches the vectors
    /// sidecar.
    fn catalog_only_root(dir: &Path) -> Config {
        let cfg = config_for(dir);
        drop(Catalog::open(&cfg.catalog_db()).expect("create catalog"));
        cfg
    }

    #[test]
    fn a_corrupt_vectors_meta_does_not_read_as_a_library_without_an_index() {
        let absent_dir = tempfile::tempdir().expect("tempdir");
        let absent = build_verify_report(&catalog_only_root(absent_dir.path()));

        let corrupt_dir = tempfile::tempdir().expect("tempdir");
        let corrupt_cfg = catalog_only_root(corrupt_dir.path());
        std::fs::create_dir_all(corrupt_cfg.lancedb_dir()).expect("create lancedb dir");
        std::fs::write(
            corrupt_cfg
                .lancedb_dir()
                .join(bookrack_vectors::meta::META_FILENAME),
            b"{ this is not a vectors meta",
        )
        .expect("write a corrupt sidecar");
        let corrupt = build_verify_report(&corrupt_cfg);

        assert_ne!(
            serde_json::to_value(&absent).expect("encode absent"),
            serde_json::to_value(&corrupt).expect("encode corrupt"),
            "a corrupt vectors_meta.json reports exactly what a library that \
             never built an index reports"
        );
        // An absent sidecar is not a failure, so the negative half has
        // to hold too: only the unreadable one carries a reason.
        assert!(
            absent.vectors_meta_error.is_none(),
            "a library that never built an index reported a meta error: {:?}",
            absent.vectors_meta_error
        );
        let reason = corrupt
            .vectors_meta_error
            .as_deref()
            .expect("a corrupt sidecar carries its reason");
        // The variant's own Display is the wrapper `vectors_meta parse
        // error`; the parser's message is what names the defect, and it
        // only survives if the chain was flattened.
        assert!(
            reason.starts_with("vectors_meta parse error: "),
            "the reason did not carry the parse error's own cause: {reason}"
        );
        // The sidecar failing says nothing about the stores, which were
        // read through their own doors.
        assert!(
            corrupt.catalog_schema_ok,
            "{:?}",
            corrupt.catalog_schema_error
        );
    }

    /// Zero the b-tree root page of the `intake` table and of every
    /// index over it, leaving the schema itself untouched. Schema
    /// verification reads `sqlite_master` and the `PRAGMA` tables, so
    /// the store still opens and verifies; every access path to the
    /// rows lands on a page whose type byte is not a b-tree.
    fn detach_intake_rows(cfg: &Config) {
        let db = cfg.catalog_db();
        let conn =
            bookrack_dbkit::open_production_strict_read_only(&db).expect("open for page lookup");
        let page_size: i64 = conn
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .expect("read page size");
        let mut stmt = conn
            .prepare(
                "SELECT rootpage FROM sqlite_master WHERE tbl_name = 'intake' AND rootpage > 0",
            )
            .expect("prepare root page query");
        let roots: Vec<i64> = stmt
            .query_map([], |row| row.get(0))
            .expect("query root pages")
            .map(|row| row.expect("root page"))
            .collect();
        assert!(!roots.is_empty(), "the intake table has no b-tree to zero");
        drop(stmt);
        drop(conn);

        let mut bytes = std::fs::read(&db).expect("read the catalog file");
        for root in roots {
            let start = ((root - 1) * page_size) as usize;
            bytes[start..start + page_size as usize].fill(0);
        }
        std::fs::write(&db, bytes).expect("write the catalog file");
    }

    #[test]
    fn an_unreadable_intake_table_carries_its_reason_rather_than_absent_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = catalog_only_root(dir.path());
        detach_intake_rows(&cfg);

        let report = build_verify_report(&cfg);

        // The store opened and its schema verified: the failure is a
        // read, not a mismatch, and the report has to say so.
        assert!(
            report.catalog_schema_ok,
            "the schema did not verify, so this is not the state under test: {:?}",
            report.catalog_schema_error
        );
        let reason = report
            .intake_scan_error
            .as_deref()
            .expect("an unreadable intake table carries its reason");
        // `CatalogError::Sqlite`'s own Display is the wrapper `catalog
        // database error`; the sqlite message is what names the defect,
        // and it only survives if the chain was flattened.
        assert!(
            reason.starts_with("catalog database error: "),
            "the reason did not carry the sqlite cause: {reason}"
        );
        assert!(
            report.intake_count.is_none() && report.missing_intake_files.is_none(),
            "counts that could not be read were reported anyway: {:?} / {:?}",
            report.intake_count,
            report.missing_intake_files
        );
    }

    #[test]
    fn a_catalog_only_root_reports_the_corpus_missing_without_creating_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_for(dir.path());
        drop(Catalog::open(&cfg.catalog_db()).expect("create catalog"));
        let before = db_files(dir.path());

        let report = build_verify_report(&cfg);
        assert!(!report.not_initialised);
        assert!(report.catalog_schema_ok);
        assert!(report.corpus_missing);
        assert!(!report.corpus_schema_ok);
        assert!(report.corpus_schema_error.is_none());
        // The negative half of the read-failure contract: a catalog
        // that reads back carries counts and no reason.
        assert_eq!(report.intake_count, Some(0));
        assert_eq!(report.missing_intake_files.as_deref(), Some(&[][..]));
        assert!(
            report.intake_scan_error.is_none(),
            "a readable catalog reported a read failure: {:?}",
            report.intake_scan_error
        );
        assert_eq!(
            db_files(dir.path()),
            before,
            "verify must not materialise corpus.db"
        );
    }
}
