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
        !names.iter().any(|n| n.starts_with("vectors/")),
        "an absent sidecar must not produce a vectors/ entry; got: {names:?}"
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
