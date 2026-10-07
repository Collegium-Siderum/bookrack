// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for [`bookrack_diagnose::collect`].
//!
//! The base fixture seeds a tempdir-backed data root with a crash
//! report, a rolling log file, a small catalog (one intake plus one
//! row of each observability table), and an empty corpus; individual
//! tests extend it with a vectors sidecar, out-of-window logs, or
//! private strings. The suite verifies the resulting tarball: it
//! lands at the expected path, every collector with a seeded source
//! contributes non-empty decodable bytes, the vectors collector
//! covers its present / absent / unreadable branches, the `--days`
//! window excludes stale logs, and the scrubber replaces private
//! paths and titles before they reach the bundle.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use bookrack_catalog::{
    ActorKind, Catalog, NewIntake, NewItemPipelineAudit, NewMcpToolCall, NewMetadataAudit,
};
use bookrack_config::Config;
use bookrack_core::ItemKind;
use bookrack_corpus::Corpus;
use bookrack_diagnose::{Options, collect};
use bookrack_test_support::{ProcessEnv, process_env};

/// A fixed unix-ms timestamp the test runs against so the bundle name
/// and the manifest's `generated_at` are reproducible.
const FROZEN_UNIX_MS: u64 = 1_717_573_200_000;

/// Isolate this binary's view of the host so the collectors' daemon-side
/// log source is the sandbox rather than the user's real per-user
/// directory. `isolated` rather than `daemon`: this crate never opens a
/// library, so it needs no embedder.
fn isolate_daemon_state_dir() -> PathBuf {
    process_env(ProcessEnv::isolated()).daemon_state_dir()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    cfg: Config,
}

impl Fixture {
    fn build() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().to_path_buf();
        std::fs::create_dir_all(data_dir.join("logs")).unwrap();

        // Seed a crash file and a rolling-log file in the data dir,
        // alongside the catalog the collectors expect.
        std::fs::write(
            data_dir.join("logs/crash-1717573000000.txt"),
            "panic: example\n",
        )
        .unwrap();
        std::fs::write(
            data_dir.join("logs/bookrack.log.2024-06-05"),
            "{\"level\":\"info\",\"msg\":\"hello\"}\n",
        )
        .unwrap();

        // Seed the catalog with one intake + one row of each audit
        // table so the catalog collector has something to write out.
        {
            let mut catalog = Catalog::open(&data_dir.join("catalog.db")).unwrap();
            catalog
                .register_intake(
                    ItemKind::Book,
                    &NewIntake::new("sha-fixture").format("epub"),
                )
                .unwrap();
            catalog
                .record_tool_call(&NewMcpToolCall::new("cli", "library.list_books", "ok"))
                .unwrap();
            catalog
                .record_pipeline_audit(&NewItemPipelineAudit::new(
                    "structure",
                    "parse_toc",
                    "ok",
                    "run-1",
                    ActorKind::Pipeline,
                ))
                .unwrap();
            let mut meta_audit =
                NewMetadataAudit::new("node_publication_attrs", "seed", ActorKind::System);
            meta_audit.node_id = Some(100_000_001);
            catalog.record_metadata_audit(&meta_audit).unwrap();
        }

        // Seed an (unstamped) corpus so the corpus collector reads a
        // real store instead of reporting a missing one.
        drop(Corpus::open(&data_dir.join("corpus.db")).unwrap());

        let cfg = Config::new(data_dir, "http://localhost:0/".to_string());
        Fixture { _tmp: tmp, cfg }
    }
}

#[test]
fn collect_writes_a_bundle_with_every_collector_present() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");
    assert!(report.scrubbed, "scrub on by default");
    // The sandbox exports a home directory, so every redaction has its
    // input and the coverage list stays empty.
    assert!(
        report.scrub_gaps.is_empty(),
        "unexpected scrub gaps: {:?}",
        report.scrub_gaps
    );
    assert!(report.files > 0);
    assert!(report.out_path.exists());

    let names = list_archive_files(&report.out_path);
    let must_contain = [
        "manifest.json",
        "env.txt",
        "crashes/crash-1717573000000.txt",
        "logs/bookrack.log.2024-06-05",
        "catalog/intakes-head.json",
        "catalog/tool-calls.json",
        "catalog/pipeline-audit.json",
        "catalog/metadata-audit.json",
        "corpus/index-meta.json",
        "papers/catalog/open-error.json",
        "papers/corpus/open-error.json",
        "refs/open-error.json",
    ];
    for needle in must_contain {
        assert!(
            names.iter().any(|n| n == needle),
            "expected {needle} in bundle; got: {names:?}"
        );
        let bytes = read_archive_file(&report.out_path, needle);
        assert!(!bytes.is_empty(), "{needle} decodes to empty bytes");
    }
    // The manifest states its own schema and the redaction coverage a
    // reader of the bundle is entitled to trust.
    let manifest_bytes = read_archive_file(&report.out_path, "manifest.json");
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(
        manifest["schema_version"],
        bookrack_diagnose::manifest::MANIFEST_SCHEMA_VERSION
    );
    assert_eq!(manifest["scrubbed"], true);
    assert_eq!(manifest["scrub_gaps"], serde_json::json!([]));
    let env_txt = String::from_utf8(read_archive_file(&report.out_path, "env.txt")).unwrap();
    assert!(
        env_txt.contains("home redaction   : applied,"),
        "env.txt must record the home-redaction source; got: {env_txt}"
    );

    // The fixture seeds no vectors sidecar — the normal fresh/legacy
    // state — so the vectors collector must contribute nothing rather
    // than an empty or error file.
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("vectors/") || n.starts_with("papers/vectors/")),
        "an absent sidecar must not produce a vectors/ entry; got: {names:?}"
    );

    // Stores a library never used are recorded as missing, so a reader
    // can tell "never ingested" from "collector skipped": the papers
    // pair and the reference store each carry that state.
    for section in ["papers/catalog", "papers/corpus", "refs"] {
        let bytes = read_archive_file(&report.out_path, &format!("{section}/open-error.json"));
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["state"], "missing", "{section}: {payload}");
    }
    // The queue document is daemon state, absent until something is
    // queued; like the sidecar it contributes nothing when absent.
    assert!(
        !names.iter().any(|n| n.starts_with("queue/")),
        "an absent queue document must not produce a queue/ entry; got: {names:?}"
    );

    // Every seeded file is intact, so no section reports a degraded
    // copy: the notes file exists only when there is something to note.
    assert!(
        !names.iter().any(|n| n.ends_with("read-notes.json")),
        "a clean run must not write a read-notes file; got: {names:?}"
    );
}

#[test]
fn collect_snapshots_the_vectors_sidecar_when_present() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    let lancedb_dir = fx.cfg.lancedb_dir();
    std::fs::create_dir_all(&lancedb_dir).unwrap();
    let meta = bookrack_vectors::meta::VectorsMeta {
        schema_version: bookrack_vectors::meta::SCHEMA_VERSION,
        min_reader_version: None,
        kind: "ivf-flat".to_string(),
        num_partitions: 64,
        num_sub_vectors: None,
        num_bits: None,
        default_nprobes: 40,
        default_refine_factor: None,
        built_at: "2024-06-01T00:00:00Z".to_string(),
        built_at_chunk_count: 123,
        churn_since_rebuild: 0,
        lance_index_name: "vector_idx".to_string(),
    };
    bookrack_vectors::meta::store(&lancedb_dir, &meta).unwrap();

    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");
    let bytes = read_archive_file(&report.out_path, "vectors/vectors_meta.json");
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot["kind"], "ivf-flat");
    assert_eq!(snapshot["built_at_chunk_count"], 123);
}

#[test]
fn collect_records_an_unreadable_vectors_sidecar_as_an_open_error() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    let lancedb_dir = fx.cfg.lancedb_dir();
    std::fs::create_dir_all(&lancedb_dir).unwrap();
    std::fs::write(lancedb_dir.join("vectors_meta.json"), "not json {").unwrap();

    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");
    let bytes = read_archive_file(&report.out_path, "vectors/open-error.json");
    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload["state"], "unreadable");
    assert_eq!(payload["store"], "vectors_meta.json");
    assert!(
        payload["error"].as_str().is_some_and(|e| !e.is_empty()),
        "the load failure must be recorded, got: {payload}"
    );
    let names = list_archive_files(&report.out_path);
    assert!(
        !names.iter().any(|n| n == "vectors/vectors_meta.json"),
        "an unreadable sidecar must not also snapshot verbatim"
    );
}

#[test]
fn logs_outside_the_days_window_are_excluded() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    let logs = fx.cfg.data_dir().join("logs");
    // `now` is frozen at 2024-06-05T07:40Z and the default window is
    // seven days, so the cutoff date is 2024-05-29: a file dated on
    // the cutoff itself stays in, one older falls out.
    std::fs::write(
        logs.join("bookrack.log.2024-05-29"),
        "{\"msg\":\"on the cutoff\"}\n",
    )
    .unwrap();
    std::fs::write(
        logs.join("bookrack.log.2024-05-20"),
        "{\"msg\":\"stale\"}\n",
    )
    .unwrap();

    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");
    let names = list_archive_files(&report.out_path);
    assert!(names.iter().any(|n| n == "logs/bookrack.log.2024-06-05"));
    assert!(names.iter().any(|n| n == "logs/bookrack.log.2024-05-29"));
    assert!(
        !names.iter().any(|n| n == "logs/bookrack.log.2024-05-20"),
        "a log older than the window must not enter the bundle; got: {names:?}"
    );
}

#[test]
fn scrub_replaces_private_paths_and_titles_inside_the_bundle() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    let data_dir = fx.cfg.data_dir().to_path_buf();
    // One JSON log line carrying the three private shapes the
    // scrubber exists for: the literal data-dir path, a book basename
    // under it, and a CJK run (escaped so no CJK bytes sit in this
    // source file).
    let cjk_title = "\u{4e66}\u{5e93}\u{76ee}\u{5f55}";
    let msg = format!(
        "ingesting {}/books/SecretTitle.pdf titled {cjk_title}",
        data_dir.display()
    );
    let line = serde_json::json!({ "level": "info", "msg": msg }).to_string();
    std::fs::write(
        data_dir.join("logs/bookrack.log.2024-06-05"),
        format!("{line}\n"),
    )
    .unwrap();

    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");
    assert!(report.scrubbed, "scrub on by default");
    let bytes = read_archive_file(&report.out_path, "logs/bookrack.log.2024-06-05");
    let body = String::from_utf8(bytes).unwrap();
    let raw_dir = data_dir.display().to_string();
    assert!(
        !body.contains(&raw_dir),
        "the literal data-dir path leaked into the bundle: {body}"
    );
    assert!(
        body.contains(bookrack_diagnose::DATA_DIR_PLACEHOLDER),
        "expected the data-dir placeholder in: {body}"
    );
    assert!(
        !body.contains("SecretTitle"),
        "a book title leaked through the path string: {body}"
    );
    assert!(
        body.contains("<file:") && body.contains(">.pdf"),
        "expected a hashed basename token in: {body}"
    );
    assert!(
        !body.contains(cjk_title),
        "a CJK run leaked into the bundle: {body}"
    );
}

/// Two CJK characters (U+7532 U+4E59) written as escapes so no CJK
/// bytes sit in this source file. Used as a stand-in title.
const CJK_PAIR: &str = "\u{7532}\u{4E59}";

/// Bytes of a log file whose tail was truncated mid-write: one intact
/// JSON record, then a plain line carrying [`CJK_PAIR`] with an
/// invalid byte wedged between its two characters.
fn truncated_log_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"{\"level\":\"info\",\"msg\":\"kept\"}\n");
    bytes.extend_from_slice("\u{7532}".as_bytes());
    bytes.push(0xff);
    bytes.extend_from_slice("\u{4E59}".as_bytes());
    bytes.push(b'\n');
    bytes
}

/// Read `<section>/read-notes.json` from the archive and return the
/// state recorded for `file`, or `None` when the file has no note.
fn read_note_state(bundle: &Path, section: &str, file: &str) -> Option<String> {
    let bytes = read_archive_file(bundle, &format!("{section}/read-notes.json"));
    let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    doc["files"]
        .as_array()
        .expect("read-notes.json carries a files array")
        .iter()
        .find(|e| e["file"] == file)
        .map(|e| e["state"].as_str().expect("a state string").to_string())
}

#[test]
fn a_non_utf8_log_file_does_not_destroy_the_bundle() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    let logs = fx.cfg.data_dir().join("logs");
    // Alongside the fixture's intact 2024-06-05 file, a second one
    // inside the same window whose tail is not valid UTF-8.
    std::fs::write(logs.join("bookrack.log.2024-06-04"), truncated_log_bytes()).unwrap();

    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("one unreadable log must not fail the bundle");

    // The intact file is unaffected by its neighbour.
    let good = read_archive_file(&report.out_path, "logs/bookrack.log.2024-06-05");
    assert!(!good.is_empty(), "the intact log must still be collected");

    // The damaged file rides through with its readable content kept.
    let body = String::from_utf8(read_archive_file(
        &report.out_path,
        "logs/bookrack.log.2024-06-04",
    ))
    .unwrap();
    assert!(
        body.contains("kept"),
        "the records before the damage must survive; got: {body}"
    );
    // The damage must not hand the scrubber two single characters
    // where it had one hashable run: rule 5 leaves a lone CJK
    // character alone, so a split run would put the title in the
    // bundle verbatim.
    assert!(
        !body.contains('\u{7532}') && !body.contains('\u{4E59}'),
        "a CJK run split by the damaged byte leaked into the bundle: {body}"
    );
    assert!(
        body.contains("<cjk:"),
        "expected the run to hash as one token; got: {body}"
    );

    // Degrading silently would hide an incomplete bundle from the very
    // reader the bundle exists for.
    assert_eq!(
        read_note_state(&report.out_path, "logs", "bookrack.log.2024-06-04"),
        Some("lossy-utf8".to_string()),
        "the damaged file must be recorded as degraded"
    );
    assert_eq!(
        read_note_state(&report.out_path, "logs", "bookrack.log.2024-06-05"),
        None,
        "an intact file must not be recorded as degraded"
    );
}

#[test]
fn a_non_utf8_crash_report_does_not_destroy_the_bundle() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    let logs = fx.cfg.data_dir().join("logs");
    // A crash report is exactly the artifact a kill signal truncates.
    std::fs::write(logs.join("crash-1717573100000.txt"), truncated_log_bytes()).unwrap();

    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("one unreadable crash must not fail the bundle");

    let intact = read_archive_file(&report.out_path, "crashes/crash-1717573000000.txt");
    assert!(
        !intact.is_empty(),
        "the intact report must still be collected"
    );

    let body = String::from_utf8(read_archive_file(
        &report.out_path,
        "crashes/crash-1717573100000.txt",
    ))
    .unwrap();
    assert!(
        body.contains("kept"),
        "the readable part of the report must survive; got: {body}"
    );
    assert!(
        !body.contains(CJK_PAIR) && !body.contains('\u{7532}'),
        "a CJK run split by the damaged byte leaked into the bundle: {body}"
    );
    assert_eq!(
        read_note_state(&report.out_path, "crashes", "crash-1717573100000.txt"),
        Some("lossy-utf8".to_string()),
        "the damaged report must be recorded as degraded"
    );
}

#[test]
fn collect_honours_no_scrub_and_writes_to_an_explicit_out_path() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("custom.tar.gz");
    let opts = Options {
        scrub: false,
        out: Some(out.clone()),
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");
    assert_eq!(report.out_path, out);
    assert!(!report.scrubbed);

    let manifest_bytes = read_archive_file(&out, "manifest.json");
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(manifest["scrubbed"], false);
    // An unredacted bundle reports no partial coverage: `scrubbed:
    // false` already says everything a reader needs.
    assert_eq!(manifest["scrub_gaps"], serde_json::json!([]));
    assert!(report.scrub_gaps.is_empty());
    let env_txt = String::from_utf8(read_archive_file(&out, "env.txt")).unwrap();
    assert!(
        env_txt.contains("home redaction   : not applicable"),
        "env.txt must not claim a redaction the run skipped; got: {env_txt}"
    );
}

#[test]
fn collect_with_an_empty_logs_dir_still_succeeds() {
    isolate_daemon_state_dir();
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    // Note: no logs/ directory and no catalog.db.
    let cfg = Config::new(data_dir, "http://localhost:0/".to_string());
    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&cfg, &opts).expect("collect must tolerate a bare data dir");
    assert!(report.out_path.exists());
    let names = list_archive_files(&report.out_path);
    assert!(names.iter().any(|n| n == "manifest.json"));
    // The database collectors record the missing stores explicitly
    // instead of omitting their sections.
    for needle in ["catalog/open-error.json", "corpus/open-error.json"] {
        assert!(
            names.iter().any(|n| n == needle),
            "expected {needle} in bundle; got: {names:?}"
        );
    }
    assert!(
        !cfg.catalog_db().exists() && !cfg.corpus_db().exists(),
        "collect must not materialise databases on a bare data dir"
    );
}

#[test]
fn collect_picks_up_the_daemon_state_dir_log_source() {
    let state_dir = isolate_daemon_state_dir();
    let state_logs = state_dir.join("logs");
    std::fs::create_dir_all(&state_logs).unwrap();
    std::fs::write(
        state_logs.join("bookrack.log.2024-06-04"),
        "{\"level\":\"info\",\"msg\":\"from the daemon state dir\"}\n",
    )
    .unwrap();
    std::fs::write(
        state_logs.join("crash-1717572000000.txt"),
        "panic: from the daemon state dir\n",
    )
    .unwrap();

    let fx = Fixture::build();
    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");
    let names = list_archive_files(&report.out_path);
    for needle in [
        // Both sources land in the bundle: the daemon state dir...
        "logs/bookrack.log.2024-06-04",
        "crashes/crash-1717572000000.txt",
        // ...and the per-root legacy location the fixture seeds.
        "logs/bookrack.log.2024-06-05",
        "crashes/crash-1717573000000.txt",
    ] {
        assert!(
            names.iter().any(|n| n == needle),
            "expected {needle} in bundle; got: {names:?}"
        );
    }
}

fn list_archive_files(path: &Path) -> Vec<String> {
    let raw = std::fs::read(path).unwrap();
    let mut decoder = flate2::read::GzDecoder::new(raw.as_slice());
    let mut tar_bytes = Vec::new();
    decoder.read_to_end(&mut tar_bytes).unwrap();
    let mut archive = tar::Archive::new(tar_bytes.as_slice());
    archive
        .entries()
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.header()
                .path()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .collect()
}

fn read_archive_file(path: &Path, name: &str) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    let mut decoder = flate2::read::GzDecoder::new(raw.as_slice());
    let mut tar_bytes = Vec::new();
    decoder.read_to_end(&mut tar_bytes).unwrap();
    let mut archive = tar::Archive::new(tar_bytes.as_slice());
    for entry in archive.entries().unwrap() {
        let mut e = entry.unwrap();
        let n = e
            .header()
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if n == name {
            let mut buf = Vec::new();
            e.read_to_end(&mut buf).unwrap();
            return buf;
        }
    }
    panic!("file not found in archive: {name}");
}

#[test]
fn collect_snapshots_the_paper_stores_when_present() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    {
        let mut catalog = Catalog::open(&fx.cfg.papers_catalog_db()).unwrap();
        catalog
            .register_intake(
                ItemKind::Paper,
                &NewIntake::new("paper-sha-fixture").format("pdf"),
            )
            .unwrap();
    }
    drop(Corpus::open(&fx.cfg.papers_corpus_db()).unwrap());
    let lancedb_dir = fx.cfg.papers_lancedb_dir();
    std::fs::create_dir_all(&lancedb_dir).unwrap();
    let meta = bookrack_vectors::meta::VectorsMeta {
        schema_version: bookrack_vectors::meta::SCHEMA_VERSION,
        min_reader_version: None,
        kind: "brute-force".to_string(),
        num_partitions: 0,
        num_sub_vectors: None,
        num_bits: None,
        default_nprobes: 0,
        default_refine_factor: None,
        built_at: "2024-06-01T00:00:00Z".to_string(),
        built_at_chunk_count: 7,
        churn_since_rebuild: 0,
        lance_index_name: "vector_idx".to_string(),
    };
    bookrack_vectors::meta::store(&lancedb_dir, &meta).unwrap();

    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");

    let bytes = read_archive_file(&report.out_path, "papers/catalog/intakes-head.json");
    let intakes: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let rows = intakes.as_array().expect("an array of intake rows");
    assert_eq!(rows.len(), 1, "{intakes}");
    assert_eq!(rows[0]["format"], "pdf", "{intakes}");
    assert_eq!(rows[0]["source_sha256"], "paper-sha-fixture", "{intakes}");

    let bytes = read_archive_file(&report.out_path, "papers/corpus/index-meta.json");
    let stamps: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        stamps["schema_version_on_disk"].is_string(),
        "a real papers corpus reports its schema version: {stamps}"
    );

    let bytes = read_archive_file(&report.out_path, "papers/vectors/vectors_meta.json");
    let sidecar: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(sidecar["built_at_chunk_count"], 7, "{sidecar}");

    // The book-side sections are untouched by the paper seeds: the
    // book catalog still carries exactly the fixture's one intake.
    let bytes = read_archive_file(&report.out_path, "catalog/intakes-head.json");
    let books: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(books.as_array().map(Vec::len), Some(1), "{books}");
}

#[test]
fn collect_summarises_the_reference_store_without_its_titles() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    {
        let refs = bookrack_refs::Refs::open(&fx.cfg.reference_db()).unwrap();
        refs.upsert_book(&bookrack_refs::NewBook {
            book_slug: "ref-fixture".to_string(),
            schema_name: "glossary".to_string(),
            schema_version: 1,
            parser_version: "parser-0".to_string(),
            title_zh: "Fixture Title Private".to_string(),
            title_en: None,
            edition: None,
            publisher: Some("Fixture Publisher Private".to_string()),
            year: Some(2001),
            isbn: None,
            authority_rank: 0,
            built_at: "2024-06-01T00:00:00Z".to_string(),
            intake_id: Some(1),
        })
        .unwrap();
        for key in ["alpha", "beta"] {
            refs.upsert_entry(&bookrack_refs::NewEntry {
                book_slug: "ref-fixture".to_string(),
                entry_key: key.to_string(),
                headword: key.to_string(),
                aliases: Vec::new(),
                payload: serde_json::json!({}),
                fts_text: key.to_string(),
                source: serde_json::json!({}),
                quality_flags: Vec::new(),
            })
            .unwrap();
        }
    }

    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");
    let bytes = read_archive_file(&report.out_path, "refs/summary.json");
    let summary: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(summary["entry_count"], 2, "{summary}");
    assert_eq!(summary["overlay_count"], 0, "{summary}");
    let books = summary["books"].as_array().expect("a books array");
    assert_eq!(books.len(), 1, "{summary}");
    assert_eq!(books[0]["book_slug"], "ref-fixture", "{summary}");
    assert_eq!(books[0]["schema_name"], "glossary", "{summary}");
    assert_eq!(books[0]["parser_version"], "parser-0", "{summary}");
    // The bibliographic columns identify a real book; the summary
    // carries the build's provenance, never those.
    let text = String::from_utf8(bytes).unwrap();
    assert!(
        !text.contains("Private"),
        "a title or publisher reached the bundle: {text}"
    );
    let names = list_archive_files(&report.out_path);
    assert!(
        !names.iter().any(|n| n == "refs/open-error.json"),
        "a readable store must not also report an open error: {names:?}"
    );
}

#[test]
fn collect_records_an_unreadable_reference_store_as_an_open_error() {
    isolate_daemon_state_dir();
    let fx = Fixture::build();
    std::fs::write(fx.cfg.reference_db(), b"this is not a database").unwrap();

    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");
    let bytes = read_archive_file(&report.out_path, "refs/open-error.json");
    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload["state"], "unreadable", "{payload}");
    assert_eq!(payload["store"], "reference.db", "{payload}");
    assert!(
        payload["error"].as_str().is_some_and(|e| !e.is_empty()),
        "the open failure must be recorded, got: {payload}"
    );
    let names = list_archive_files(&report.out_path);
    assert!(
        !names.iter().any(|n| n == "refs/summary.json"),
        "an unreadable store must not also summarise: {names:?}"
    );
}

/// One pending job whose source sits under the data root, so the
/// snapshot has a path for the scrubber to fold.
fn queue_document_with_one_job(data_dir: &Path) -> String {
    use bookrack_core::queue::{JobState, Priority, QueueJob, QueueState};
    let state = QueueState {
        jobs: vec![QueueJob {
            id: "0190f9b0-0000-7000-8000-000000000001".to_string(),
            library: "fixture-library".to_string(),
            path: data_dir.join("inbox").join("sample.epub"),
            priority: Priority::Normal,
            force: false,
            hold_for_metadata: false,
            kind: ItemKind::Book,
            intake_ocr: None,
            audit_profile: None,
            state: JobState::Pending,
            queued_at: chrono::DateTime::from_timestamp_millis(FROZEN_UNIX_MS as i64).unwrap(),
            started_at: None,
            finished_at: None,
            error: None,
            merged_into: None,
        }],
        ..QueueState::default()
    };
    serde_json::to_string_pretty(&state).unwrap()
}

#[test]
fn collect_snapshots_the_queue_document_and_reports_a_refused_one() {
    let state_dir = isolate_daemon_state_dir();
    std::fs::create_dir_all(&state_dir).unwrap();
    let queue_path = state_dir.join("queue.json");
    let fx = Fixture::build();
    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };

    std::fs::write(&queue_path, queue_document_with_one_job(fx.cfg.data_dir())).unwrap();
    let report = collect(&fx.cfg, &opts).expect("collect");
    let bytes = read_archive_file(&report.out_path, "queue/queue.json");
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        snapshot["schema_version"],
        bookrack_core::queue::QUEUE_SCHEMA_VERSION,
        "{snapshot}"
    );
    let jobs = snapshot["jobs"].as_array().expect("a jobs array");
    assert_eq!(jobs.len(), 1, "{snapshot}");
    assert_eq!(jobs[0]["library"], "fixture-library", "{snapshot}");
    let path = jobs[0]["path"].as_str().expect("the job path is a string");
    assert!(
        path.starts_with(bookrack_diagnose::DATA_DIR_PLACEHOLDER),
        "the job's source path must be scrubbed like every other path: {path}"
    );
    assert!(
        !path.contains("sample"),
        "the source basename is a title and must be folded: {path}"
    );

    // A document a newer binary wrote is left alone by the daemon and
    // must be reported as refused here, with both versions named.
    let newer = bookrack_core::queue::QUEUE_SCHEMA_VERSION + 1;
    std::fs::write(
        &queue_path,
        format!("{{\"schema_version\": {newer}, \"paused\": false, \"jobs\": []}}"),
    )
    .unwrap();
    let report = collect(&fx.cfg, &opts).expect("collect");
    let bytes = read_archive_file(&report.out_path, "queue/open-error.json");
    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload["state"], "unreadable", "{payload}");
    assert_eq!(payload["store"], "queue.json", "{payload}");
    let error = payload["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(&newer.to_string()),
        "the refusal must name the version on disk: {payload}"
    );
    let names = list_archive_files(&report.out_path);
    assert!(
        !names.iter().any(|n| n == "queue/queue.json"),
        "a refused document must not also snapshot: {names:?}"
    );
    std::fs::remove_file(&queue_path).unwrap();
}
