// SPDX-License-Identifier: Apache-2.0

//! Terminal implementation of [`WizardDriver`].
//!
//! Reads stdin for prompts, writes progress to stdout, errors to
//! stderr. Owns every operator-facing string of the wizard; the
//! runner hands over structured reports only.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bookrack_embed::ProbeReport as EmbedProbeReport;

use super::runner::validate_unused_or_force;
use super::{
    DataRootHint, FinalizeSummary, OllamaStep, PdfiumChoice, PdfiumInstallOutcome, PdfiumReport,
    SmokeOutcome, WizardDriver,
};

pub struct CliWizardDriver {
    /// Mirrors `WizardOpts::non_interactive`: suppresses every prompt
    /// this driver would otherwise issue after step 1.
    pub non_interactive: bool,
}

#[async_trait::async_trait]
impl WizardDriver for CliWizardDriver {
    /// Step 1: pick the data root.
    ///
    /// In interactive mode this is the only prompt the wizard issues. A
    /// `bookrack-data` directory beside the running binary is offered
    /// as the Press-Enter default (the portable layout); otherwise the
    /// prompt has no default and the operator types a path. Validation
    /// runs before the choice is echoed, so a refused root never
    /// renders a `Using` line.
    async fn step_data_root(&self, hint: DataRootHint) -> Result<PathBuf> {
        print_intro();
        println!("[1/5] Data root");
        if let Some(path) = &hint.data_dir {
            let abs = absolutise(path)?;
            validate_unused_or_force(&abs, hint.force)?;
            println!("      Using {}", abs.display());
            return Ok(abs);
        }
        if hint.non_interactive {
            anyhow::bail!("--data-dir is required in --non-interactive mode");
        }
        let typed = match &hint.portable {
            Some(p) => {
                println!("      Portable layout detected at {}.", p.display(),);
                prompt_line("      Press Enter to use it, or type another path: ")?
            }
            None => prompt_line("      Where should books, indexes, and logs live? Path: ")?,
        };
        let chosen = if typed.is_empty() {
            hint.portable
                .context("a data root path is required (no portable layout to default to)")?
        } else {
            PathBuf::from(typed)
        };
        let abs = absolutise(&chosen)?;
        validate_unused_or_force(&abs, hint.force)?;
        println!("      Using {}", abs.display());
        Ok(abs)
    }

    /// Step 2: report the PDFium search. Warn-only: ingest of EPUB and
    /// TXT works without PDFium; only the PDF adapter needs it. A miss
    /// lists every directory the loader would check; when a pinned
    /// binary exists for this platform, an interactive run offers to
    /// download it on the spot.
    async fn step_pdfium(&self, report: &PdfiumReport) -> Result<PdfiumChoice> {
        println!("[2/5] PDFium native library");
        if let Some(path) = &report.found {
            println!("      Found {}", path.display());
            return Ok(PdfiumChoice::Continue);
        }
        let filename = report.filename;
        println!("      WARN: {filename} not found. Searched:");
        for dir in &report.probed {
            println!("            {}", dir.display());
        }
        if report.installable && !self.non_interactive {
            let answer = prompt_line("      Download the pinned PDFium build now? [Y/n]: ")?;
            if answer.is_empty() || answer.eq_ignore_ascii_case("y") {
                println!("      Downloading ...");
                return Ok(PdfiumChoice::Install);
            }
        }
        println!(
            "            Run `bookrack doctor --install-pdfium` later, or set \
             BOOKRACK_PDFIUM_LIB. PDF ingest will fail until the library is \
             present; EPUB and TXT still work."
        );
        Ok(PdfiumChoice::Continue)
    }

    /// Step 2b: report the download outcome. Warn-only either way; a
    /// failed install degrades PDF ingest, nothing else.
    async fn step_pdfium_install(&self, outcome: &PdfiumInstallOutcome) -> Result<()> {
        match outcome {
            PdfiumInstallOutcome::Installed(path) => {
                println!("      Installed {}", path.display());
            }
            PdfiumInstallOutcome::Failed(reason) => {
                eprintln!("      WARN: PDFium install failed: {reason}");
                eprintln!(
                    "            Run `bookrack doctor --install-pdfium` to retry. \
                     PDF ingest will fail until the library is present; EPUB \
                     and TXT still work."
                );
            }
        }
        Ok(())
    }

    /// Step 3: report the Ollama probe. Unreachable daemon or a
    /// missing embed model aborts the wizard with a remediation hint.
    async fn step_ollama(&self, step: &OllamaStep<'_>) -> Result<()> {
        let url = step.url;
        let embed_model = step.embed_model;
        println!("[3/5] Ollama daemon");
        println!("      Probing {url} ...");
        if !step.report.reachable {
            eprintln!("      FAIL: Ollama is not reachable at {url}.");
            eprintln!("            Install it from https://ollama.com, run `ollama serve`,");
            eprintln!("            pull the model:");
            eprintln!("              ollama pull {embed_model}");
            eprintln!("            then rerun `bookrack init`.");
            anyhow::bail!("Ollama unreachable");
        }
        if !report_has_model(step.report, embed_model) {
            eprintln!("      FAIL: Ollama is up but {embed_model} is not pulled.");
            eprintln!("            Run:  ollama pull {embed_model}");
            eprintln!("            then rerun `bookrack init`.");
            anyhow::bail!("embed model not pulled");
        }
        println!(
            "      OK ({} model(s) pulled, {embed_model} present)",
            step.report.models.len(),
        );
        Ok(())
    }

    /// Step 4: report the smoke outcome. A zero-hit search aborts —
    /// the embed or search pipeline is broken end-to-end.
    async fn step_smoke(&self, outcome: &SmokeOutcome) -> Result<()> {
        println!("[4/5] End-to-end probe");
        match outcome {
            SmokeOutcome::Skipped => {
                println!("      Skipped (--no-smoke).");
            }
            SmokeOutcome::Ran(report) => {
                println!("      Ingesting a synthetic fixture through Ollama -> LanceDB ...");
                println!(
                    "      Ingested {} chunk(s); querying for marker ...",
                    report.chunks_written,
                );
                if report.hits == 0 {
                    let marker = report.marker_query;
                    anyhow::bail!(
                        "smoke search returned no hits for `{marker}` -- the embed or search pipeline is broken"
                    );
                }
                println!("      OK ({} hit(s) on the marker token)", report.hits);
            }
        }
        Ok(())
    }

    /// Step 5: report what finalize wrote, then the closing hints.
    async fn step_finalize(&self, summary: &FinalizeSummary) -> Result<()> {
        println!("[5/5] Finalizing");
        println!(
            "      Created {} (sources, books, logs, audit-rules)",
            summary.data_root.display()
        );
        if summary.config_kept {
            println!("      Kept existing {}", summary.config_path.display());
        } else {
            println!("      Wrote {}", summary.config_path.display());
        }
        match &summary.registry {
            Some(path) => {
                println!("      Wrote {} (default = \"default\")", path.display());
            }
            None => {
                eprintln!(
                    "      WARN: could not locate the platform config directory. \
                     Set BOOKRACK_DATA_DIR=\"{}\" so other shells find this library.",
                    summary.data_root.display(),
                );
            }
        }
        print_success(&summary.data_root);
        Ok(())
    }
}

fn print_intro() {
    println!("bookrack init: a five-step setup wizard.");
    println!();
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

fn report_has_model(probe: &EmbedProbeReport, name: &str) -> bool {
    probe.models.iter().any(|m| m == name)
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

/// Print a prompt to stdout and read a single trimmed line from stdin.
fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush().context("flush stdout")?;
    let stdin = std::io::stdin();
    let mut buf = String::new();
    stdin.lock().read_line(&mut buf).context("read line")?;
    Ok(buf.trim().to_string())
}
