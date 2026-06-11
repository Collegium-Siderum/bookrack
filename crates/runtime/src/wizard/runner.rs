// SPDX-License-Identifier: Apache-2.0

//! The wizard state machine and its on-disk side effects.
//!
//! Owns the fixed step order, the probes, the tempdir smoke run, and
//! every write that lands in the real data root. All rendering and
//! abort decisions that belong to the operator-facing surface live in
//! the [`WizardDriver`] implementations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{
    Config, DEFAULT_EMBED_MODEL, DEFAULT_OLLAMA_URL, EMBED_MODEL_ENV, EmbedConfig, OLLAMA_URL_ENV,
    ROOT_CONFIG_NAME, default_registry_path, locate_pdfium, merge_library_into_registry,
    pdfium_library_filename, portable_data_dir,
};
use bookrack_corpus::Corpus;
use bookrack_embed::{OllamaEmbedClient, probe_ollama};
use bookrack_ingest::{IngestParams, ingest_book};
use bookrack_ops::{Caller, Ops, SearchOptions, reads};
use bookrack_query::Library;

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

        let summary = finalize(&data_root, &url, &embed_model, opts.force)?;
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
/// use, through the existing env-var conventions ([`OLLAMA_URL_ENV`]
/// and [`EMBED_MODEL_ENV`]). The wizard never writes either to
/// `config.toml` from the env value, only from whichever value the
/// finalize step was told to record.
fn resolve_ollama_target() -> (String, String) {
    let url = std::env::var(OLLAMA_URL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string());
    let model = std::env::var(EMBED_MODEL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EMBED_MODEL.to_string());
    (url, model)
}

/// Refuse to write into an existing populated data root unless the
/// operator passed `force`. The marker tested is `catalog.db`: an
/// empty directory the user just made by hand is fine, but a directory
/// that already holds a library must not be silently re-init'd.
pub(super) fn validate_unused_or_force(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !path.is_dir() {
        anyhow::bail!("{} exists but is not a directory", path.display());
    }
    if path.join("catalog.db").exists() && !force {
        anyhow::bail!(
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
        anyhow::bail!("smoke ingest produced zero chunks");
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
fn finalize(
    data_root: &Path,
    ollama_url: &str,
    embed_model: &str,
    force: bool,
) -> Result<FinalizeSummary> {
    create_data_root_skeleton(data_root)?;
    let (config_path, config_kept) = write_root_config(data_root, ollama_url, embed_model, force)?;
    let registry = write_default_registry(data_root)?;
    Ok(FinalizeSummary {
        data_root: data_root.to_path_buf(),
        config_path,
        config_kept,
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
fn write_root_config(
    data_root: &Path,
    ollama_url: &str,
    embed_model: &str,
    force: bool,
) -> Result<(PathBuf, bool)> {
    let path = data_root.join(ROOT_CONFIG_NAME);
    if path.exists() && !force {
        return Ok((path, true));
    }
    let contents = format!(
        "# bookrack root config. Written by `bookrack init`; safe to edit.\n\
         ollama_url = \"{ollama_url}\"\n\
         embed_model = \"{embed_model}\"\n"
    );
    std::fs::write(&path, contents).with_context(|| format!("write {}", path.display()))?;
    Ok((path, false))
}

/// Merge `default = <data_root>` into the platform-default registry.
/// Returns `None` when no platform config directory could be located;
/// the driver renders the `BOOKRACK_DATA_DIR` fallback hint.
fn write_default_registry(data_root: &Path) -> Result<Option<PathBuf>> {
    let Some(path) = default_registry_path() else {
        return Ok(None);
    };
    merge_library_into_registry(&path, "default", data_root)
        .with_context(|| format!("merge {} into registry", data_root.display()))?;
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
    fn write_root_config_creates_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (path, kept) =
            write_root_config(tmp.path(), "http://x:1", "model:xyz", false).expect("write");
        assert!(!kept, "fresh write should not report kept");
        assert_eq!(path, tmp.path().join(ROOT_CONFIG_NAME));
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("ollama_url = \"http://x:1\""));
        assert!(text.contains("embed_model = \"model:xyz\""));
    }

    #[test]
    fn write_root_config_keeps_existing_without_force() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(ROOT_CONFIG_NAME);
        std::fs::write(&path, "hand-edited content").expect("seed");
        let (_, kept) = write_root_config(tmp.path(), "ignored", "ignored", false).expect("noop");
        assert!(kept, "existing config should be kept");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text, "hand-edited content");
    }

    #[test]
    fn write_root_config_overwrites_with_force() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(ROOT_CONFIG_NAME);
        std::fs::write(&path, "hand-edited content").expect("seed");
        let (_, kept) =
            write_root_config(tmp.path(), "http://new:9", "new-model", true).expect("force");
        assert!(!kept, "force overwrite should not report kept");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("http://new:9"));
        assert!(text.contains("new-model"));
    }
}
