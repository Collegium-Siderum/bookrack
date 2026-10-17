// SPDX-License-Identifier: Apache-2.0

//! `bookrack verify` — per-store schema and on-disk file checks. The
//! report is built here and serialised by the `verify.run` control-plane
//! method; nothing in this module prints.

use std::path::{Path, PathBuf};

use bookrack_catalog::{Catalog, IntakeFilter};
use bookrack_config::Config;
use bookrack_core::ItemKind;
use bookrack_corpus::Corpus;
use bookrack_refs::Refs;
use eyre::{Context, Result};

/// Per-store findings the `verify.run` control-plane method returns:
/// the book pipeline's fields at the top level, the paper pipeline's
/// under `papers`, and the reference store's beside them. Every
/// `*_error` field holds the failure's full source chain, flattened, so
/// the report names the defect and not only the layer that noticed it.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct VerifyReport {
    /// Set when the data directory holds none of the five databases —
    /// the two book stores, the two paper stores, `reference.db`.
    /// Verify short-circuits in that case and reports nothing else.
    pub not_initialised: bool,
    /// The book pipeline: `catalog.db`, `corpus.db`, `lancedb/`, with
    /// intake files under `books/`. Serialised at the top level.
    #[serde(flatten)]
    pub books: PipelineFindings,
    /// The paper pipeline: `papers_catalog.db`, `papers_corpus.db`,
    /// `lancedb_papers/`, with intake files under `papers/`. Both
    /// stores missing is a library that never ingested a paper.
    pub papers: PipelineFindings,
    /// Set when `reference.db` is absent: a library with no distilled
    /// reference book.
    pub reference_missing: bool,
    /// Set when `reference.db` opened read-only and carries the schema
    /// version this build reads. The table shapes are not verified.
    pub reference_schema_ok: bool,
    /// Why `reference.db` did not open read-only at this build's
    /// schema version, flattened to its full source chain.
    pub reference_schema_error: Option<String>,
}

/// One pipeline's findings: its catalog and corpus, the intake files
/// its catalog names, and its vector sidecar. An unverifiable store
/// leaves its schema flag false and its error populated, and the
/// counts that depend on it absent.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct PipelineFindings {
    /// Set when the catalog is absent; the store is reported missing
    /// rather than opened into existence.
    pub catalog_missing: bool,
    /// Set when the corpus is absent.
    pub corpus_missing: bool,
    pub catalog_schema_ok: bool,
    pub catalog_schema_error: Option<String>,
    pub corpus_schema_ok: bool,
    pub corpus_schema_error: Option<String>,
    pub intake_count: Option<u64>,
    pub missing_intake_files: Option<Vec<i64>>,
    /// Why the intake rows could not be read, flattened to its full
    /// source chain. Set when the catalog opened and its schema
    /// verified but a read of the table failed, which leaves the two
    /// counts above absent. Distinct from `intake_count` being
    /// `Some(0)`, which is a readable catalog holding no intakes.
    pub intake_scan_error: Option<String>,
    pub vectors_built_at_chunk_count: Option<u64>,
    pub vectors_churn: Option<u64>,
    /// Why `vectors_meta.json` could not be read, flattened to its full
    /// source chain. Distinct from all three counts being absent, which
    /// is a pipeline that never built an ANN index.
    pub vectors_meta_error: Option<String>,
}

impl PipelineFindings {
    /// Presence of the two stores, before either is opened.
    fn probe(paths: &PipelinePaths) -> Self {
        Self {
            catalog_missing: !paths.catalog_db.exists(),
            corpus_missing: !paths.corpus_db.exists(),
            ..Self::default()
        }
    }

    /// True when neither store is on disk: a pipeline the library
    /// never used.
    pub fn absent(&self) -> bool {
        self.catalog_missing && self.corpus_missing
    }
}

/// Where one pipeline keeps its stores and intake files.
struct PipelinePaths {
    kind: ItemKind,
    catalog_db: PathBuf,
    corpus_db: PathBuf,
    lancedb_dir: PathBuf,
    intake_root: PathBuf,
}

/// Collect verifiable findings for every store under `cfg`. Each
/// database is probed by file presence first and opened through its
/// read-only door only when present, so verify neither materialises a
/// missing store nor takes the write lock a live daemon holds. A data
/// directory with none of the five databases is reported as
/// `not_initialised`; one store of a pipeline present without the
/// other reports the absent one as missing instead of inventing it.
pub fn build_verify_report(cfg: &Config) -> VerifyReport {
    let books = PipelinePaths {
        kind: ItemKind::Book,
        catalog_db: cfg.catalog_db(),
        corpus_db: cfg.corpus_db(),
        lancedb_dir: cfg.lancedb_dir(),
        intake_root: cfg.books_dir(),
    };
    let papers = PipelinePaths {
        kind: ItemKind::Paper,
        catalog_db: cfg.papers_catalog_db(),
        corpus_db: cfg.papers_corpus_db(),
        lancedb_dir: cfg.papers_lancedb_dir(),
        intake_root: cfg.papers_dir(),
    };
    let mut report = VerifyReport {
        books: PipelineFindings::probe(&books),
        papers: PipelineFindings::probe(&papers),
        reference_missing: !cfg.reference_db().exists(),
        ..Default::default()
    };
    if report.books.absent() && report.papers.absent() && report.reference_missing {
        report.not_initialised = true;
        return report;
    }

    verify_pipeline(&mut report.books, &books);
    verify_pipeline(&mut report.papers, &papers);
    if !report.reference_missing {
        // The door checks the version stamp in both directions; the
        // error variants carry the found and supported versions.
        match Refs::open_read_only(&cfg.reference_db()) {
            Ok(_) => report.reference_schema_ok = true,
            Err(e) => report.reference_schema_error = Some(bookrack_core::error_chain(&e)),
        }
    }
    report
}

/// Fill `findings` for one pipeline whose store presence is already
/// probed: open each present store through its read-only door, scan
/// the intake files its catalog names, and read its vector sidecar.
fn verify_pipeline(findings: &mut PipelineFindings, paths: &PipelinePaths) {
    // Schema verification happens inside the open paths; surface success
    // as a one-liner per database, and any failure as a multi-line block.
    if !findings.catalog_missing {
        match Catalog::open_read_only(&paths.catalog_db) {
            Ok(catalog) => {
                findings.catalog_schema_ok = true;
                // A store whose schema verifies can still fail to be
                // read. Both probes go through the same connection, so
                // the first reason is kept and the second is dropped
                // rather than overwriting it.
                match catalog.count_intakes() {
                    Ok(count) => findings.intake_count = Some(count),
                    // The variants are wrappers, so the chain is
                    // flattened before it crosses the RPC boundary.
                    Err(e) => findings.intake_scan_error = Some(bookrack_core::error_chain(&e)),
                }
                match scan_intake_files(&catalog, paths.kind, &paths.intake_root) {
                    Ok(missing) => findings.missing_intake_files = Some(missing),
                    Err(e) => {
                        findings.intake_scan_error.get_or_insert(format!("{e:#}"));
                    }
                }
            }
            // `Verify` is a wrapper variant whose Display names the
            // store alone; the per-table differences are its source.
            Err(e) => {
                findings.catalog_schema_error = Some(bookrack_core::error_chain(&e));
            }
        }
    }
    if !findings.corpus_missing {
        match Corpus::open_read_only(&paths.corpus_db) {
            Ok(_) => {
                findings.corpus_schema_ok = true;
            }
            Err(e) => {
                findings.corpus_schema_error = Some(bookrack_core::error_chain(&e));
            }
        }
    }
    // `load` already separates an absent sidecar (`Ok(None)`, a
    // pipeline that never built an index) from an unreadable one. Keep
    // the two apart in the report: the error variants are wrappers, so
    // the chain is flattened before it crosses the RPC boundary.
    let vectors_meta = match bookrack_vectors::meta::load(&paths.lancedb_dir) {
        Ok(meta) => meta,
        Err(e) => {
            findings.vectors_meta_error = Some(bookrack_core::error_chain(&e));
            None
        }
    };
    if let Some(meta) = &vectors_meta {
        findings.vectors_built_at_chunk_count = Some(meta.built_at_chunk_count);
        findings.vectors_churn = Some(meta.churn_since_rebuild);
    }
}

/// Walk every intake row, resolve its `stored_path` under
/// `intake_root`, and return the intake ids whose file is missing. An
/// absolute `stored_path` resolves to itself. `Err` is returned only
/// when the catalog could not be enumerated.
fn scan_intake_files(catalog: &Catalog, kind: ItemKind, intake_root: &Path) -> Result<Vec<i64>> {
    let filter = IntakeFilter {
        kind,
        ..IntakeFilter::default()
    };
    let intakes = catalog
        .find_intakes(&filter, u32::MAX, 0)
        .context("enumerate intakes")?;
    let mut missing = Vec::new();
    for intake in intakes {
        let Some(stored) = intake.stored_path else {
            continue;
        };
        let resolved = intake_root.join(&stored);
        if !resolved.exists() {
            missing.push(intake.intake_id);
        }
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_catalog::NewIntake;
    use bookrack_core::ItemKind;
    use bookrack_extract::envelope::envelope_filename;
    use bookrack_refs::Refs;
    use serde_json::{Value, json};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

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
        assert!(report.books.catalog_missing);
        assert!(report.books.corpus_missing);
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
            absent.books.vectors_meta_error.is_none(),
            "a library that never built an index reported a meta error: {:?}",
            absent.books.vectors_meta_error
        );
        let reason = corrupt
            .books
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
            corrupt.books.catalog_schema_ok,
            "{:?}",
            corrupt.books.catalog_schema_error
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
            report.books.catalog_schema_ok,
            "the schema did not verify, so this is not the state under test: {:?}",
            report.books.catalog_schema_error
        );
        let reason = report
            .books
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
            report.books.intake_count.is_none() && report.books.missing_intake_files.is_none(),
            "counts that could not be read were reported anyway: {:?} / {:?}",
            report.books.intake_count,
            report.books.missing_intake_files
        );
    }

    /// Rename a column of the `intake` table in the catalog at `db` so
    /// the live schema no longer matches its `TableSpec`; the store
    /// still opens as a database, and it is verification that refuses
    /// it.
    fn rename_intake_column(db: &Path) {
        let conn = bookrack_dbkit::open_production(db).expect("open for the rename");
        conn.execute_batch("ALTER TABLE intake RENAME COLUMN stored_path TO stored_path_x")
            .expect("rename the column");
    }

    #[test]
    fn a_schema_mismatch_carries_the_column_differences() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = catalog_only_root(dir.path());
        rename_intake_column(&cfg.catalog_db());

        let report = build_verify_report(&cfg);

        assert!(!report.books.catalog_schema_ok);
        let reason = report
            .books
            .catalog_schema_error
            .as_deref()
            .expect("a schema that does not verify carries its reason");
        // `CatalogError::Verify`'s own Display is the wrapper `catalog
        // schema verification failed`; the per-table differences are
        // what name the defect, and they only survive if the chain was
        // flattened.
        assert!(
            reason.starts_with("catalog schema verification failed: "),
            "the reason did not carry the verifier's own report: {reason}"
        );
        assert!(
            reason.contains("table `intake`"),
            "the reason did not name the table that failed: {reason}"
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
        assert!(report.books.catalog_schema_ok);
        assert!(report.books.corpus_missing);
        assert!(!report.books.corpus_schema_ok);
        assert!(report.books.corpus_schema_error.is_none());
        // The negative half of the read-failure contract: a catalog
        // that reads back carries counts and no reason.
        assert_eq!(report.books.intake_count, Some(0));
        assert_eq!(report.books.missing_intake_files.as_deref(), Some(&[][..]));
        assert!(
            report.books.intake_scan_error.is_none(),
            "a readable catalog reported a read failure: {:?}",
            report.books.intake_scan_error
        );
        assert_eq!(
            db_files(dir.path()),
            before,
            "verify must not materialise corpus.db"
        );
    }

    /// The report as it crosses the RPC boundary.
    fn report_json(cfg: &Config) -> Value {
        serde_json::to_value(build_verify_report(cfg)).expect("encode the report")
    }

    /// A paper-only data root under `dir`: both paper stores and one
    /// paper intake whose `stored_path` is the absolute envelope path
    /// glean records. The envelope itself is written only when
    /// `file_present`. Returns the intake id and the envelope path.
    fn papers_only_root(dir: &Path, file_present: bool) -> (Config, i64, PathBuf) {
        let cfg = config_for(dir);
        let mut catalog = Catalog::open(&cfg.papers_catalog_db()).expect("create papers catalog");
        let intake_id = catalog
            .register_intake(ItemKind::Paper, &NewIntake::new("sha-paper"))
            .expect("register a paper")
            .into_intake()
            .intake_id;
        let envelope = cfg
            .papers_dir()
            .join(envelope_filename(ItemKind::Paper, intake_id));
        catalog
            .set_stored_path(
                ItemKind::Paper,
                intake_id,
                envelope.to_string_lossy().as_ref(),
            )
            .expect("record the stored path");
        drop(catalog);
        drop(Corpus::open(&cfg.papers_corpus_db()).expect("create papers corpus"));
        if file_present {
            std::fs::create_dir_all(cfg.papers_dir()).expect("papers dir");
            std::fs::write(&envelope, b"{}").expect("write the envelope");
        }
        (cfg, intake_id, envelope)
    }

    #[test]
    fn a_papers_only_root_is_verified_rather_than_reported_uninitialised() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cfg, _, _) = papers_only_root(dir.path(), true);
        let before = db_files(dir.path());

        let report = report_json(&cfg);

        assert_eq!(
            report["not_initialised"], false,
            "a root holding the paper stores is not uninitialised: {report}"
        );
        let papers = &report["papers"];
        assert_eq!(papers["catalog_schema_ok"], true, "{report}");
        assert_eq!(papers["corpus_schema_ok"], true, "{report}");
        assert_eq!(papers["intake_count"], 1, "{report}");
        assert_eq!(papers["missing_intake_files"], json!([]), "{report}");
        // The book side is absent as a pair, with nothing invented for it.
        assert_eq!(report["catalog_missing"], true, "{report}");
        assert_eq!(report["corpus_missing"], true, "{report}");
        assert_eq!(report["catalog_schema_error"], Value::Null, "{report}");
        assert_eq!(report["corpus_schema_error"], Value::Null, "{report}");
        assert_eq!(
            db_files(dir.path()),
            before,
            "verify must not materialise the book stores"
        );
    }

    #[test]
    fn a_paper_whose_stored_file_is_gone_is_reported_under_papers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cfg, intake_id, envelope) = papers_only_root(dir.path(), true);
        assert!(
            envelope.is_absolute(),
            "the fixture must record the absolute path glean writes"
        );

        // One row, one stored path; the file is the only variable.
        let present = report_json(&cfg);
        std::fs::remove_file(&envelope).expect("remove the envelope");
        let gone = report_json(&cfg);

        assert_eq!(
            gone["papers"]["missing_intake_files"],
            json!([intake_id]),
            "a paper whose stored file is gone is not named: {gone}"
        );
        assert_eq!(
            present["papers"]["missing_intake_files"],
            json!([]),
            "{present}"
        );
        let mut expected = present.clone();
        expected["papers"]["missing_intake_files"] = json!([intake_id]);
        assert_eq!(
            gone, expected,
            "removing the file changed more of the report than the missing list"
        );
    }

    #[test]
    fn a_paper_store_that_does_not_verify_carries_its_own_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cfg, _, _) = papers_only_root(dir.path(), true);
        rename_intake_column(&cfg.papers_catalog_db());

        let report = report_json(&cfg);

        let papers = &report["papers"];
        let reason = papers["catalog_schema_error"].as_str().unwrap_or_else(|| {
            panic!("a paper catalog that does not verify carries its reason: {report}")
        });
        assert!(
            reason.starts_with("catalog schema verification failed: ")
                && reason.contains("table `intake`"),
            "the reason did not carry the verifier's own report: {reason}"
        );
        assert_eq!(papers["catalog_schema_ok"], false, "{report}");
        assert_eq!(papers["corpus_schema_ok"], true, "{report}");
        assert_eq!(
            report["catalog_schema_error"],
            Value::Null,
            "a paper-side failure leaked onto the book side: {report}"
        );
    }

    #[test]
    fn half_a_paper_pipeline_reports_the_absent_store_without_creating_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_for(dir.path());
        drop(Catalog::open(&cfg.papers_catalog_db()).expect("create papers catalog"));
        let before = db_files(dir.path());

        let report = report_json(&cfg);

        assert_eq!(report["not_initialised"], false, "{report}");
        let papers = &report["papers"];
        assert_eq!(papers["catalog_missing"], false, "{report}");
        assert_eq!(papers["corpus_missing"], true, "{report}");
        assert_eq!(papers["catalog_schema_ok"], true, "{report}");
        assert_eq!(papers["corpus_schema_error"], Value::Null, "{report}");
        assert_eq!(
            db_files(dir.path()),
            before,
            "verify must not materialise papers_corpus.db"
        );
    }

    #[test]
    fn a_reference_only_root_is_verified_rather_than_reported_uninitialised() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_for(dir.path());
        drop(Refs::open(&cfg.reference_db()).expect("create reference.db"));
        let before = db_files(dir.path());

        let report = report_json(&cfg);

        assert_eq!(
            report["not_initialised"], false,
            "a root holding reference.db is not uninitialised: {report}"
        );
        assert_eq!(report["reference_missing"], false, "{report}");
        assert_eq!(report["reference_schema_ok"], true, "{report}");
        assert_eq!(report["reference_schema_error"], Value::Null, "{report}");
        for (side, findings) in [("books", &report), ("papers", &report["papers"])] {
            assert_eq!(findings["catalog_missing"], true, "{side}: {report}");
            assert_eq!(findings["corpus_missing"], true, "{side}: {report}");
        }
        assert_eq!(
            db_files(dir.path()),
            before,
            "verify must not materialise a store"
        );
    }

    #[test]
    fn a_reference_store_at_another_schema_version_carries_its_reason() {
        for (found, phrase) in [
            (0, "is behind this build"),
            (
                bookrack_refs::TARGET_VERSION + 1,
                "is newer than this build",
            ),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let cfg = config_for(dir.path());
            drop(Refs::open(&cfg.reference_db()).expect("create reference.db"));
            let conn = bookrack_dbkit::open_production(&cfg.reference_db()).expect("open");
            conn.pragma_update(None, "user_version", found)
                .expect("stamp the version");
            drop(conn);

            let report = report_json(&cfg);

            let reason = report["reference_schema_error"]
                .as_str()
                .unwrap_or_else(|| {
                    panic!("reference.db at user_version {found} carries no reason: {report}")
                });
            assert!(
                reason.contains(phrase) && reason.contains(&format!("user_version {found}")),
                "the reason did not name the version refusal: {reason}"
            );
            assert_eq!(report["reference_schema_ok"], false, "{report}");
            assert_eq!(report["not_initialised"], false, "{report}");
        }
    }

    #[test]
    fn the_book_side_fields_keep_their_top_level_names_types_and_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config_for(dir.path());
        let mut catalog = Catalog::open(&cfg.catalog_db()).expect("create catalog");
        let intake_id = catalog
            .register_intake(ItemKind::Book, &NewIntake::new("sha-book"))
            .expect("register a book")
            .into_intake()
            .intake_id;
        catalog
            .set_stored_path(ItemKind::Book, intake_id, "aa/source.txt")
            .expect("record the stored path");
        drop(catalog);
        drop(Corpus::open(&cfg.corpus_db()).expect("create corpus"));
        let stored = cfg.books_dir().join("aa/source.txt");
        std::fs::create_dir_all(stored.parent().expect("parent")).expect("books dir");
        std::fs::write(&stored, b"source bytes").expect("write the stored file");
        std::fs::create_dir_all(cfg.lancedb_dir()).expect("lancedb dir");
        bookrack_vectors::meta::store(
            &cfg.lancedb_dir(),
            &bookrack_vectors::meta::VectorsMeta {
                schema_version: bookrack_vectors::meta::SCHEMA_VERSION,
                min_reader_version: None,
                kind: "ivf-flat".to_string(),
                num_partitions: 4,
                num_sub_vectors: None,
                num_bits: None,
                default_nprobes: 2,
                default_refine_factor: None,
                built_at: "2026-01-01T00:00:00Z".to_string(),
                built_at_chunk_count: 7,
                churn_since_rebuild: 3,
                lance_index_name: bookrack_vectors::meta::DEFAULT_INDEX_NAME.to_string(),
            },
        )
        .expect("write the vectors sidecar");

        let wire = report_json(&cfg);

        // The wire contract `verify.run` carried before the paper and
        // reference fields: these names stay top-level, with these
        // types and, for this library, these values.
        let book_side = json!({
            "not_initialised": false,
            "catalog_missing": false,
            "corpus_missing": false,
            "catalog_schema_ok": true,
            "catalog_schema_error": null,
            "corpus_schema_ok": true,
            "corpus_schema_error": null,
            "intake_count": 1,
            "missing_intake_files": [],
            "intake_scan_error": null,
            "vectors_built_at_chunk_count": 7,
            "vectors_churn": 3,
            "vectors_meta_error": null,
        });
        let book_side = book_side.as_object().expect("object");
        let object = wire.as_object().expect("the report is a JSON object");
        for (key, value) in book_side {
            assert_eq!(object.get(key), Some(value), "top-level `{key}`: {wire}");
        }

        let added = [
            "papers",
            "reference_missing",
            "reference_schema_ok",
            "reference_schema_error",
        ];
        let expected_keys: BTreeSet<&str> =
            book_side.keys().map(String::as_str).chain(added).collect();
        let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
        assert_eq!(keys, expected_keys, "top-level keys: {wire}");

        // The paper side carries the book side's per-pipeline fields
        // under the same names; a root without papers reports the pair
        // absent and nothing else.
        let papers = wire["papers"].as_object().expect("`papers` is an object");
        let pipeline_keys: BTreeSet<&str> = book_side
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "not_initialised")
            .collect();
        let paper_keys: BTreeSet<&str> = papers.keys().map(String::as_str).collect();
        assert_eq!(paper_keys, pipeline_keys, "`papers` keys: {wire}");
        assert_eq!(wire["papers"]["catalog_missing"], true, "{wire}");
        assert_eq!(wire["papers"]["corpus_missing"], true, "{wire}");
        assert_eq!(wire["reference_missing"], true, "{wire}");
        assert_eq!(wire["reference_schema_ok"], false, "{wire}");
        assert_eq!(wire["reference_schema_error"], Value::Null, "{wire}");

        // The client decodes the same bytes back into the same report.
        let decoded: VerifyReport =
            serde_json::from_value(wire.clone()).expect("decode the report");
        assert_eq!(
            serde_json::to_value(&decoded).expect("re-encode"),
            wire,
            "the report does not survive the client's decoder"
        );
    }
}
