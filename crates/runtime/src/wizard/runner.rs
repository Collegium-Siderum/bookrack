// SPDX-License-Identifier: Apache-2.0

//! The wizard state machine and its on-disk side effects.
//!
//! Owns the fixed step order, the probes, the tempdir smoke run, and
//! every write that lands in the real data root. All rendering and
//! abort decisions that belong to the operator-facing surface live in
//! the [`WizardDriver`] implementations.

use std::path::{Path, PathBuf};

use bookrack_catalog::Catalog;
use bookrack_config::{
    Config, DEFAULT_OLLAMA_URL, EmbedConfig, LibraryEntryFields, LibraryKind, LibraryManifest,
    MANIFEST_FILENAME, OLLAMA_URL_ENV, ROOT_CONFIG_NAME, load_manifest, locate_pdfium,
    new_manifest, pdfium_library_filename, portable_data_dir, registry_target_path,
    render_root_config_toml, upsert_library_entry, write_manifest,
};

use bookrack_corpus::Corpus;
use bookrack_embed::{OllamaEmbedClient, probe_ollama};
use bookrack_ingest::{IngestParams, ingest_book};
use bookrack_ops::{Caller, Ops, SearchOptions, reads};
use bookrack_query::Library;
use eyre::{Context, Result};

use super::{
    DataRootHint, FinalizeSummary, OllamaStep, PdfiumChoice, PdfiumInstallOutcome, PdfiumReport,
    SmokeOutcome, SmokeReport, WizardDriver,
};

/// Synthetic fixture the smoke step ingests. Carries a unique marker
/// token so a query for it is guaranteed to hit this very chunk.
const SMOKE_FIXTURE: &[u8] = include_bytes!("fixtures/smoke.txt");

/// Marker token in the fixture. The smoke step queries for this and
/// expects at least one hit; zero hits indicates the embed -> search
/// pipeline is broken end-to-end.
const SMOKE_QUERY: &str = "grommet-zarpkin-3147";

/// Subdirectories every data root carries, created by both the smoke
/// tempdir and the real root.
const SKELETON_SUBDIRS: [&str; 4] = ["sources", "books", "logs", "audit-rules"];

/// Driver-independent wizard parameters. The CLI builds one from its
/// parsed flags; a GUI builds one from its settings form.
#[derive(Debug, Clone, Default)]
pub struct WizardOpts {
    /// Accept an existing data root that already holds a `catalog.db`.
    /// Without this flag the wizard refuses, so a misconfigured run
    /// cannot silently graft itself onto a populated library.
    pub force: bool,
    /// Skip the end-to-end smoke step. Useful when developing the
    /// wizard itself or when Ollama is intentionally offline.
    pub no_smoke: bool,
    /// Skip every prompt. Requires `data_dir`. Suitable for scripted
    /// installs and CI; an interactive operator should leave it off.
    pub non_interactive: bool,
    /// Where the library's data root should live. When `None`, the
    /// driver picks (interactively or by refusing).
    pub data_dir: Option<PathBuf>,
}

pub struct Wizard;

impl Wizard {
    /// Run the five steps in fixed order. The driver's `Err` is the
    /// only abort path; the runner does not retry or jump back.
    pub async fn run<D: WizardDriver>(driver: &D, opts: WizardOpts) -> Result<()> {
        let hint = DataRootHint {
            portable: portable_data_dir(),
            data_dir: opts.data_dir.clone(),
            non_interactive: opts.non_interactive,
            force: opts.force,
        };
        let data_root = driver.step_data_root(hint).await?;
        validate_unused_or_force(&data_root, opts.force)?;

        let pdfium = probe_pdfium();
        if driver.step_pdfium(&pdfium).await? == PdfiumChoice::Install {
            let outcome = match crate::pdfium_install::install_pinned_pdfium().await {
                Ok(path) => PdfiumInstallOutcome::Installed(path),
                Err(e) => PdfiumInstallOutcome::Failed(format!("{e:#}")),
            };
            driver.step_pdfium_install(&outcome).await?;
        }

        let (url, embed_model) = resolve_ollama_target();
        let report = probe_ollama(&url).await.context("probe Ollama")?;
        driver
            .step_ollama(&OllamaStep {
                url: &url,
                embed_model: &embed_model,
                report: &report,
            })
            .await?;

        let outcome = if opts.no_smoke {
            SmokeOutcome::Skipped
        } else {
            let report = run_smoke(&url, &embed_model).await?;
            SmokeOutcome::Ran(report)
        };
        driver.step_smoke(&outcome).await?;

        let summary = finalize(&data_root, &url, opts.force)?;
        driver.step_finalize(&summary).await?;
        Ok(())
    }
}

/// Step 2 probe: walk the PDFium library search chain and note whether
/// a pinned binary could be installed for this platform.
fn probe_pdfium() -> PdfiumReport {
    let filename = pdfium_library_filename();
    let location = locate_pdfium();
    PdfiumReport {
        filename,
        found: location.dir.map(|d| d.join(filename)),
        probed: location.probed,
        installable: bookrack_extract::pdfium_pin::pinned_pdfium_binary().is_some(),
    }
}

/// Resolve the Ollama URL and embed model that downstream steps will
/// use.
///
/// The URL follows the [`OLLAMA_URL_ENV`] convention; the wizard never
/// writes it to `config.toml` from the env value, only from whichever
/// value the finalize step was told to record.
///
/// The model goes through [`EmbedConfig::resolve`], the same chain every
/// other embed path uses, so what the probe and the smoke test exercise
/// is what the library will actually embed with. A root being
/// initialized references no index profile yet, so the chain lands on
/// the default; routing through it anyway keeps the wizard from growing
/// a second, drifting definition of the model.
fn resolve_ollama_target() -> (String, String) {
    let url = std::env::var(OLLAMA_URL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string());
    (url, EmbedConfig::resolve(None).model)
}

/// Refuse to write into an existing populated data root unless the
/// operator passed `force`. The marker tested is `catalog.db`: an
/// empty directory the user just made by hand is fine, but a directory
/// that already holds a library must not be silently re-init'd.
pub(super) fn validate_unused_or_force(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !path.is_dir() {
        eyre::bail!("{} exists but is not a directory", path.display());
    }
    if path.join("catalog.db").exists() && !force {
        eyre::bail!(
            "{} looks like an existing bookrack data root (catalog.db present); \
             pass --force to use it",
            path.display(),
        );
    }
    Ok(())
}

/// Step 4: ingest a synthetic fixture into a tempdir and query for its
/// marker token. Proves the embed and search pipeline runs end-to-end
/// on this host before the wizard writes anything to the real data
/// root. The tempdir is `RemoveOnDrop`, so a failure here leaves no
/// residue regardless of where it fails. A zero-hit search is not an
/// error at this layer — the driver decides.
async fn run_smoke(ollama_url: &str, embed_model: &str) -> Result<SmokeReport> {
    let tmpdir = tempfile::tempdir().context("create smoke tempdir")?;
    let smoke_root = tmpdir.path().to_path_buf();
    for sub in SKELETON_SUBDIRS {
        std::fs::create_dir_all(smoke_root.join(sub))
            .with_context(|| format!("create smoke skeleton dir {sub}"))?;
    }
    let fixture_path = smoke_root.join("smoke.txt");
    std::fs::write(&fixture_path, SMOKE_FIXTURE).context("write smoke fixture")?;

    let cfg = Config::new(smoke_root.clone(), ollama_url.to_string());
    let embed_cfg = EmbedConfig {
        model: embed_model.to_string(),
        ..Default::default()
    };

    let report = smoke_ingest(&cfg, &embed_cfg, &fixture_path).await?;
    if report.chunks_written == 0 {
        eyre::bail!("smoke ingest produced zero chunks");
    }

    let hits = smoke_search(&cfg, &embed_cfg).await?;
    Ok(SmokeReport {
        chunks_written: report.chunks_written,
        hits,
        marker_query: SMOKE_QUERY,
    })
}

async fn smoke_ingest(
    cfg: &Config,
    embed_cfg: &EmbedConfig,
    fixture: &Path,
) -> Result<bookrack_ingest::IngestReport> {
    let mut corpus = Corpus::open(&cfg.corpus_db()).context("open smoke corpus")?;
    let mut catalog = Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir())
        .context("open smoke catalog")?;
    let embedder = build_embedder(cfg, embed_cfg)?;
    let params = IngestParams {
        embed: embed_cfg.clone(),
        ..Default::default()
    };
    ingest_book(
        fixture,
        &mut corpus,
        &mut catalog,
        &cfg.lancedb_dir(),
        &cfg.books_dir(),
        &embedder,
        &params,
    )
    .await
    .context("smoke ingest")
}

async fn smoke_search(cfg: &Config, embed_cfg: &EmbedConfig) -> Result<usize> {
    let embedder = build_embedder(cfg, embed_cfg)?;
    let library = Library::open(
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        embedder,
        embed_cfg.model.clone(),
        1,
        bookrack_ingest::CHUNK_VERSION,
    )
    .await
    .context("open smoke library")?;
    let ops = Ops::with_library(
        library,
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
        cfg.books_dir(),
        cfg.backup_dir(),
        Caller::cli(),
    );
    let hits = reads::search::search(&ops, SMOKE_QUERY, SearchOptions::default(), None)
        .await
        .context("smoke search")?;
    Ok(hits.len())
}

fn build_embedder(cfg: &Config, embed_cfg: &EmbedConfig) -> Result<OllamaEmbedClient> {
    OllamaEmbedClient::new(
        cfg.ollama_url(),
        &embed_cfg.model,
        embed_cfg.request_timeout,
        embed_cfg.max_retries,
        embed_cfg.backoff_base,
    )
    .context("build smoke embedder")
}

/// Step 5: create the real data root, write `<data_root>/config.toml`,
/// and merge a pointer into the platform-default registry. Existing
/// files are kept untouched unless `force` is set, so a rerun of the
/// wizard against the same root does not stomp on edits the operator
/// made by hand.
fn finalize(data_root: &Path, ollama_url: &str, force: bool) -> Result<FinalizeSummary> {
    create_data_root_skeleton(data_root)?;
    let (config_path, config_kept) = write_root_config(data_root, ollama_url, force)?;
    let (manifest, manifest_kept) = ensure_library_manifest(data_root, "default")?;
    let registry = write_default_registry(data_root, &manifest)?;
    Ok(FinalizeSummary {
        data_root: data_root.to_path_buf(),
        config_path,
        config_kept,
        manifest_path: data_root.join(MANIFEST_FILENAME),
        manifest_kept,
        registry,
    })
}

fn create_data_root_skeleton(data_root: &Path) -> Result<()> {
    for sub in SKELETON_SUBDIRS {
        let path = data_root.join(sub);
        std::fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
    }
    Ok(())
}

/// Write `<data_root>/config.toml` unless one already exists and
/// `force` is off. Returns the config path and whether the existing
/// file was kept.
fn write_root_config(data_root: &Path, ollama_url: &str, force: bool) -> Result<(PathBuf, bool)> {
    let path = data_root.join(ROOT_CONFIG_NAME);
    if path.exists() && !force {
        return Ok((path, true));
    }
    let contents = render_root_config_toml(ollama_url);
    std::fs::write(&path, contents).with_context(|| format!("write {}", path.display()))?;
    Ok((path, false))
}

/// Ensure the data root carries an identity manifest, returning it. An
/// existing manifest is preserved so the library's uuid stays stable
/// across reruns — the manifest is identity, not configuration, so
/// `force` never regenerates it; only a root without one gets a freshly
/// generated uuid. The bool reports whether an existing manifest was
/// kept.
fn ensure_library_manifest(data_root: &Path, name: &str) -> Result<(LibraryManifest, bool)> {
    if let Some(existing) = load_manifest(data_root)
        .with_context(|| format!("read manifest in {}", data_root.display()))?
    {
        return Ok((existing, true));
    }
    let manifest = new_manifest(name, LibraryKind::Prod, None);
    write_manifest(data_root, &manifest)
        .with_context(|| format!("write manifest in {}", data_root.display()))?;
    Ok((manifest, false))
}

/// Register `default = <data_root>` in the registry the write-side
/// commands target (`BOOKRACK_REGISTRY` when set, else the
/// platform-default file), caching the manifest's identity fields in
/// the entry. Returns `None` when no registry path could be resolved;
/// the driver renders the `BOOKRACK_DATA_DIR` fallback hint.
fn write_default_registry(data_root: &Path, manifest: &LibraryManifest) -> Result<Option<PathBuf>> {
    let Some(path) = registry_target_path() else {
        return Ok(None);
    };
    let entry = LibraryEntryFields {
        data_dir: data_root.to_path_buf(),
        kind: manifest.kind,
        description: manifest.description.clone(),
        index_profile: None,
        created_at: manifest.created_at.clone(),
        uuid: Some(manifest.uuid.clone()),
    };
    upsert_library_entry(&path, "default", &entry)
        .with_context(|| format!("register {} into registry", data_root.display()))?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_fixture_contains_the_query_marker() {
        let text = std::str::from_utf8(SMOKE_FIXTURE).expect("fixture is UTF-8");
        assert!(
            text.contains(SMOKE_QUERY),
            "fixture must carry the marker the smoke step searches for"
        );
    }

    #[test]
    fn validate_unused_or_force_accepts_an_empty_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        validate_unused_or_force(tmp.path(), false).expect("empty dir is fine");
    }

    #[test]
    fn validate_unused_or_force_accepts_a_missing_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("not-yet-created");
        validate_unused_or_force(&target, false).expect("missing dir is fine");
    }

    #[test]
    fn validate_unused_or_force_refuses_populated_root_without_force() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("catalog.db"), b"fake").expect("seed catalog");
        let err = validate_unused_or_force(tmp.path(), false).expect_err("should refuse");
        let rendered = format!("{err}");
        assert!(
            rendered.contains("--force"),
            "missing --force hint: {rendered}"
        );
    }

    #[test]
    fn validate_unused_or_force_accepts_populated_root_with_force() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("catalog.db"), b"fake").expect("seed catalog");
        validate_unused_or_force(tmp.path(), true).expect("--force overrides");
    }

    #[test]
    fn validate_unused_or_force_refuses_a_non_directory_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("not-a-dir");
        std::fs::write(&path, b"").expect("seed file");
        let err = validate_unused_or_force(&path, false).expect_err("should refuse");
        let rendered = format!("{err}");
        assert!(
            rendered.contains("not a directory"),
            "missing reason: {rendered}"
        );
    }

    #[test]
    fn create_data_root_skeleton_creates_each_subdir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("library");
        create_data_root_skeleton(&root).expect("skeleton");
        for sub in SKELETON_SUBDIRS {
            assert!(root.join(sub).is_dir(), "{sub} not created");
        }
    }

    #[test]
    fn ensure_library_manifest_writes_a_new_manifest_and_preserves_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (first, kept) = ensure_library_manifest(tmp.path(), "default").expect("first");
        assert!(!kept, "a fresh root writes a new manifest");
        assert_eq!(first.name, "default");
        assert_eq!(first.kind, LibraryKind::Prod);
        assert!(load_manifest(tmp.path()).expect("load").is_some());
        // A rerun keeps the same identity — the uuid never regenerates.
        let (second, kept) = ensure_library_manifest(tmp.path(), "renamed").expect("second");
        assert!(kept, "an existing manifest is kept");
        assert_eq!(second.uuid, first.uuid);
        assert_eq!(second.name, "default");
    }

    #[test]
    fn write_root_config_creates_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (path, kept) = write_root_config(tmp.path(), "http://x:1", false).expect("write");
        assert!(!kept, "fresh write should not report kept");
        assert_eq!(path, tmp.path().join(ROOT_CONFIG_NAME));
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("ollama_url = \"http://x:1\""));
        // The embed model is the index profile's fact, and `embed_model`
        // is a retired key: writing one would produce a root the very
        // next command refuses to load.
        assert!(!text.contains("embed_model"), "{text}");
        bookrack_config::load_root_config(tmp.path()).expect("a freshly written root loads");
    }

    #[test]
    fn write_root_config_keeps_existing_without_force() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(ROOT_CONFIG_NAME);
        std::fs::write(&path, "hand-edited content").expect("seed");
        let (_, kept) = write_root_config(tmp.path(), "ignored", false).expect("noop");
        assert!(kept, "existing config should be kept");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text, "hand-edited content");
    }

    /// `ollama_url` carrying TOML-significant characters used to slip
    /// through `format!` unescaped: a `"` closed the basic string and
    /// the rest of the line became invalid TOML, so the next daemon
    /// start failed with `RootConfigMalformed`. Serializing through
    /// the `toml` crate now escapes them, and `load_root_config`
    /// reads back exactly what the wizard was handed.
    #[test]
    fn write_root_config_round_trips_special_characters_in_ollama_url() {
        let tricky = "http://x:1/has \"quotes\" and a\nnewline and a \\ and a \u{2028}";
        let tmp = tempfile::tempdir().expect("tempdir");
        write_root_config(tmp.path(), tricky, false).expect("write");
        let loaded = bookrack_config::load_root_config(tmp.path()).expect("load");
        assert_eq!(loaded.ollama_url.as_deref(), Some(tricky));
    }

    /// A wider sweep of single tricky characters: each one used to be
    /// a candidate for breaking the hand-rolled TOML body.
    #[test]
    fn write_root_config_round_trips_each_tricky_character_alone() {
        for ch in [
            '"', '\\', '\n', '\r', '\t', '\u{2028}', '\u{2029}', '\u{0000}',
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let url = format!("http://x:1/{ch}");
            write_root_config(tmp.path(), &url, true).expect("write");
            let parsed = bookrack_config::load_root_config(tmp.path())
                .unwrap_or_else(|e| panic!("char {ch:?} broke parse: {e}"));
            assert_eq!(parsed.ollama_url.as_deref(), Some(url.as_str()));
        }
    }

    #[test]
    fn write_root_config_overwrites_with_force() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(ROOT_CONFIG_NAME);
        std::fs::write(&path, "hand-edited content").expect("seed");
        let (_, kept) = write_root_config(tmp.path(), "http://new:9", true).expect("force");
        assert!(!kept, "force overwrite should not report kept");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("http://new:9"));
    }
}
