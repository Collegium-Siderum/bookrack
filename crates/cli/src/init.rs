// SPDX-License-Identifier: Apache-2.0

//! `bookrack init`: the interactive install wizard.
//!
//! Five steps, each one written so the operator sees what is being
//! checked, why it might fail, and what to do about it. The first three
//! steps make no on-disk changes. Step four exercises the full
//! ingest -> embed -> query pipeline against a tempdir, leaving the
//! user's chosen data root untouched until validation passes. Step five
//! creates the real data root, writes `<data_root>/config.toml`, and
//! merges a pointer into the platform-default registry.
//!
//! The wizard dispatches before `Config::resolve` runs — the resolver
//! errors out on an unconfigured install, which is the very state init
//! is meant to fix.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::{
    Config, DEFAULT_EMBED_MODEL, DEFAULT_OLLAMA_URL, EMBED_MODEL_ENV, EmbedConfig, OLLAMA_URL_ENV,
    ROOT_CONFIG_NAME, default_registry_path, merge_library_into_registry, pdfium_lib_dir,
    portable_data_dir,
};
use bookrack_corpus::Corpus;
use bookrack_embed::{OllamaEmbedClient, ProbeReport, probe_ollama};
use bookrack_ingest::{IngestParams, ingest_book};
use bookrack_ops::{Caller, Ops, SearchOptions, reads};
use bookrack_query::Library;

/// Synthetic fixture the smoke step ingests. Carries a unique marker
/// token so a query for it is guaranteed to hit this very chunk.
const SMOKE_FIXTURE: &[u8] = include_bytes!("fixtures/smoke.txt");

/// Marker token in the fixture. The smoke step queries for this and
/// expects at least one hit; zero hits indicates the embed -> search
/// pipeline is broken end-to-end.
const SMOKE_QUERY: &str = "grommet-zarpkin-3147";

/// CLI-shape parameters for [`run`]. The clap layer in `main.rs` builds
/// one of these from the parsed flags.
#[derive(Debug, Clone)]
pub struct Args {
    /// Where the library's data root should live. When `None` in
    /// interactive mode, the wizard prompts; in non-interactive mode,
    /// this is required.
    pub data_dir: Option<PathBuf>,
    /// Skip every prompt. Requires `data_dir`. Suitable for scripted
    /// installs and CI; an interactive operator should leave it off.
    pub non_interactive: bool,
    /// Accept an existing data root that already holds a `catalog.db`.
    /// Without this flag the wizard refuses, so a misconfigured run
    /// cannot silently graft itself onto a populated library.
    pub force: bool,
    /// Skip the end-to-end smoke step. Useful when developing the
    /// wizard itself or when Ollama is intentionally offline.
    pub no_smoke: bool,
}

/// Run the wizard. Reads stdin for prompts, writes progress to stdout,
/// errors to stderr.
pub async fn run(args: Args) -> Result<()> {
    print_intro();

    let data_root = step_data_root(&args)?;
    step_pdfium_library();
    let (ollama_url, embed_model) = step_ollama().await?;
    if args.no_smoke {
        println!("[4/5] End-to-end probe");
        println!("      Skipped (--no-smoke).");
    } else {
        step_smoke(&ollama_url, &embed_model).await?;
    }
    step_finalize(&data_root, &ollama_url, &embed_model, args.force)?;

    print_success(&data_root);
    Ok(())
}

fn print_intro() {
    println!("bookrack init: a five-step setup wizard.");
    println!();
}

/// Step 1: pick the data root.
///
/// In interactive mode this is the only prompt the wizard issues. A
/// `bookrack-data` directory beside the running binary is offered as
/// the Press-Enter default (the portable layout); otherwise the prompt
/// has no default and the operator types a path.
fn step_data_root(args: &Args) -> Result<PathBuf> {
    println!("[1/5] Data root");
    if let Some(path) = &args.data_dir {
        let abs = absolutise(path)?;
        validate_unused_or_force(&abs, args.force)?;
        println!("      Using {}", abs.display());
        return Ok(abs);
    }
    if args.non_interactive {
        anyhow::bail!("--data-dir is required in --non-interactive mode");
    }
    let portable = portable_data_dir();
    let typed = match &portable {
        Some(p) => {
            println!("      Portable layout detected at {}.", p.display(),);
            prompt_line("      Press Enter to use it, or type another path: ")?
        }
        None => prompt_line("      Where should books, indexes, and logs live? Path: ")?,
    };
    let chosen = if typed.is_empty() {
        portable.context("a data root path is required (no portable layout to default to)")?
    } else {
        PathBuf::from(typed)
    };
    let abs = absolutise(&chosen)?;
    validate_unused_or_force(&abs, args.force)?;
    println!("      Using {}", abs.display());
    Ok(abs)
}

/// Resolve a user-typed path against the current working directory.
/// Relative paths are common in interactive use; the wizard records the
/// absolute form so a later `bookrack` invocation from another
/// directory still finds the same root.
fn absolutise(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let cwd = std::env::current_dir().context("read current working directory")?;
    Ok(cwd.join(p))
}

/// Refuse to write into an existing populated data root unless the
/// operator passed `--force`. The marker we test is `catalog.db`: an
/// empty directory the user just made by hand is fine, but a directory
/// that already holds a library must not be silently re-init'd.
fn validate_unused_or_force(path: &Path, force: bool) -> Result<()> {
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

/// Step 2: probe the PDFium library next to the binary.
///
/// Warn-only: ingest of EPUB and TXT works without PDFium; only the PDF
/// adapter needs it. A missing library is reported with the exact path
/// the loader would look at so the operator can drop the file there.
fn step_pdfium_library() {
    println!("[2/5] PDFium native library");
    let dir = pdfium_lib_dir();
    let filename = pdfium_filename();
    let path = dir.join(filename);
    if path.is_file() {
        println!("      Found {}", path.display());
    } else {
        println!("      WARN: {} not found.", path.display());
        println!(
            "            Drop {filename} there, or set BOOKRACK_PDFIUM_LIB. \
             PDF ingest will fail until you do; EPUB and TXT still work."
        );
    }
}

/// Platform-conventional filename of the PDFium dynamic library.
fn pdfium_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else {
        "libpdfium.so"
    }
}

/// Step 3: probe Ollama for liveness and the configured embed model.
///
/// Returns the URL and model that downstream steps will use. Both are
/// resolved through the existing env-var conventions
/// ([`OLLAMA_URL_ENV`] and [`EMBED_MODEL_ENV`]); the wizard never
/// writes either to `config.toml` from the env value, only from
/// whichever value step five was told to record.
async fn step_ollama() -> Result<(String, String)> {
    println!("[3/5] Ollama daemon");
    let url = std::env::var(OLLAMA_URL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string());
    let embed_model = std::env::var(EMBED_MODEL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EMBED_MODEL.to_string());
    println!("      Probing {url} ...");
    let report = probe_ollama(&url).await.context("probe Ollama")?;
    if !report.reachable {
        eprintln!("      FAIL: Ollama is not reachable at {url}.");
        eprintln!("            Install it from https://ollama.com, run `ollama serve`,");
        eprintln!("            pull the model:");
        eprintln!("              ollama pull {embed_model}");
        eprintln!("            then rerun `bookrack init`.");
        anyhow::bail!("Ollama unreachable");
    }
    if !report_has_model(&report, &embed_model) {
        eprintln!("      FAIL: Ollama is up but {embed_model} is not pulled.");
        eprintln!("            Run:  ollama pull {embed_model}");
        eprintln!("            then rerun `bookrack init`.");
        anyhow::bail!("embed model not pulled");
    }
    println!(
        "      OK ({} model(s) pulled, {embed_model} present)",
        report.models.len(),
    );
    Ok((url, embed_model))
}

fn report_has_model(probe: &ProbeReport, name: &str) -> bool {
    probe.models.iter().any(|m| m == name)
}

/// Step 4: ingest a synthetic fixture into a tempdir and query for its
/// marker token. Proves the embed and search pipeline runs end-to-end
/// on this host before the wizard writes anything to the real data
/// root. The tempdir is `RemoveOnDrop`, so a failure here leaves no
/// residue regardless of where it fails.
async fn step_smoke(ollama_url: &str, embed_model: &str) -> Result<()> {
    println!("[4/5] End-to-end probe");
    println!("      Ingesting a synthetic fixture through Ollama -> LanceDB ...");

    let tmpdir = tempfile::tempdir().context("create smoke tempdir")?;
    let smoke_root = tmpdir.path().to_path_buf();
    for sub in ["sources", "books", "logs", "audit-rules"] {
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
    println!(
        "      Ingested {} chunk(s); querying for marker ...",
        report.chunks_written,
    );

    let hits = smoke_search(&cfg, &embed_cfg).await?;
    if hits == 0 {
        anyhow::bail!(
            "smoke search returned no hits for `{SMOKE_QUERY}` -- the embed or search pipeline is broken"
        );
    }
    println!("      OK ({hits} hit(s) on the marker token)");
    Ok(())
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
    )
    .await
    .context("open smoke library")?;
    let ops = Ops::with_library(
        library,
        cfg.corpus_db(),
        cfg.catalog_db(),
        &cfg.lancedb_dir(),
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
/// files are kept untouched unless `--force` is set, so a rerun of
/// `bookrack init` against the same root does not stomp on edits the
/// operator made by hand.
fn step_finalize(data_root: &Path, ollama_url: &str, embed_model: &str, force: bool) -> Result<()> {
    println!("[5/5] Finalizing");
    create_data_root_skeleton(data_root)?;
    write_root_config(data_root, ollama_url, embed_model, force)?;
    write_default_registry(data_root)?;
    Ok(())
}

fn create_data_root_skeleton(data_root: &Path) -> Result<()> {
    for sub in ["sources", "books", "logs", "audit-rules"] {
        let path = data_root.join(sub);
        std::fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
    }
    println!(
        "      Created {} (sources, books, logs, audit-rules)",
        data_root.display()
    );
    Ok(())
}

fn write_root_config(
    data_root: &Path,
    ollama_url: &str,
    embed_model: &str,
    force: bool,
) -> Result<()> {
    let path = data_root.join(ROOT_CONFIG_NAME);
    if path.exists() && !force {
        println!("      Kept existing {}", path.display());
        return Ok(());
    }
    let contents = format!(
        "# bookrack root config. Written by `bookrack init`; safe to edit.\n\
         ollama_url = \"{ollama_url}\"\n\
         embed_model = \"{embed_model}\"\n"
    );
    std::fs::write(&path, contents).with_context(|| format!("write {}", path.display()))?;
    println!("      Wrote {}", path.display());
    Ok(())
}

fn write_default_registry(data_root: &Path) -> Result<()> {
    let Some(path) = default_registry_path() else {
        eprintln!(
            "      WARN: could not locate the platform config directory. \
             Set BOOKRACK_DATA_DIR=\"{}\" so other shells find this library.",
            data_root.display(),
        );
        return Ok(());
    };
    merge_library_into_registry(&path, "default", data_root)
        .with_context(|| format!("merge {} into registry", data_root.display()))?;
    println!("      Wrote {} (default = \"default\")", path.display());
    Ok(())
}

fn print_success(data_root: &Path) {
    println!();
    println!("bookrack is ready.");
    println!();
    println!("Data root: {}", data_root.display());
    println!();
    println!("Try:");
    println!("  bookrack ingest /path/to/book.epub");
    println!("  bookrack query \"your question\"");
    println!("  bookrack-mcp          # start the MCP server on 127.0.0.1:8765");
}

/// Print a prompt to stdout and read a single trimmed line from stdin.
fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush().context("flush stdout")?;
    let stdin = std::io::stdin();
    let mut buf = String::new();
    stdin.lock().read_line(&mut buf).context("read line")?;
    Ok(buf.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdfium_filename_is_platform_specific() {
        let name = pdfium_filename();
        if cfg!(target_os = "windows") {
            assert_eq!(name, "pdfium.dll");
        } else if cfg!(target_os = "macos") {
            assert_eq!(name, "libpdfium.dylib");
        } else {
            assert_eq!(name, "libpdfium.so");
        }
    }

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
        for sub in ["sources", "books", "logs", "audit-rules"] {
            assert!(root.join(sub).is_dir(), "{sub} not created");
        }
    }

    #[test]
    fn write_root_config_creates_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_root_config(tmp.path(), "http://x:1", "model:xyz", false).expect("write");
        let text = std::fs::read_to_string(tmp.path().join(ROOT_CONFIG_NAME)).expect("read");
        assert!(text.contains("ollama_url = \"http://x:1\""));
        assert!(text.contains("embed_model = \"model:xyz\""));
    }

    #[test]
    fn write_root_config_keeps_existing_without_force() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(ROOT_CONFIG_NAME);
        std::fs::write(&path, "hand-edited content").expect("seed");
        write_root_config(tmp.path(), "ignored", "ignored", false).expect("noop");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text, "hand-edited content");
    }

    #[test]
    fn write_root_config_overwrites_with_force() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(ROOT_CONFIG_NAME);
        std::fs::write(&path, "hand-edited content").expect("seed");
        write_root_config(tmp.path(), "http://new:9", "new-model", true).expect("force");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("http://new:9"));
        assert!(text.contains("new-model"));
    }
}
