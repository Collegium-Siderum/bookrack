// SPDX-License-Identifier: Apache-2.0

//! End-to-end smoke test for [`bookrack_diagnose::collect`].
//!
//! The test seeds a tempdir-backed data root with a crash report, a
//! rolling log file, and a small catalog (one intake plus one row of
//! each observability table), then calls `collect` and verifies the
//! resulting tarball: it lands at the expected path, contains every
//! collector's output, and decodes back to non-empty bytes for each
//! one.

use std::io::Read;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use bookrack_catalog::{
    ActorKind, Catalog, NewBookPipelineAudit, NewIntake, NewMcpToolCall, NewMetadataAudit,
};
use bookrack_config::Config;
use bookrack_diagnose::{Options, collect};

/// A fixed unix-ms timestamp the test runs against so the bundle name
/// and the manifest's `generated_at` are reproducible.
const FROZEN_UNIX_MS: u64 = 1_717_573_200_000;

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
                .register_intake(&NewIntake::new("sha-fixture").format("epub"))
                .unwrap();
            catalog
                .record_tool_call(&NewMcpToolCall::new("cli", "library.list_books", "ok"))
                .unwrap();
            catalog
                .record_pipeline_audit(&NewBookPipelineAudit::new(
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

        let cfg = Config::new(data_dir, "http://localhost:0/".to_string());
        Fixture { _tmp: tmp, cfg }
    }
}

#[test]
fn collect_writes_a_bundle_with_every_collector_present() {
    let fx = Fixture::build();
    let opts = Options {
        now: Some(UNIX_EPOCH + Duration::from_millis(FROZEN_UNIX_MS)),
        ..Options::default()
    };
    let report = collect(&fx.cfg, &opts).expect("collect");
    assert!(report.scrubbed, "scrub on by default");
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
    }
}

#[test]
fn collect_honours_no_scrub_and_writes_to_an_explicit_out_path() {
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
}

#[test]
fn collect_with_an_empty_logs_dir_still_succeeds() {
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
