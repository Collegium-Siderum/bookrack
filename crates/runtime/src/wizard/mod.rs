// SPDX-License-Identifier: Apache-2.0

//! First-run wizard, driver-agnostic.
//!
//! [`Wizard::run`] walks a fixed five-step state machine; every
//! interactive surface goes through [`WizardDriver`]. The first three
//! steps make no on-disk changes. Step four exercises the full
//! ingest -> embed -> query pipeline against a tempdir, leaving the
//! chosen data root untouched until validation passes. Step five
//! creates the real data root, writes `<data_root>/config.toml`, and
//! merges a pointer into the platform-default registry.
//!
//! `CliWizardDriver` is the terminal implementation; a GUI front end
//! implements the same trait to drive the identical probes and writes.

use std::path::PathBuf;

use bookrack_embed::ProbeReport as EmbedProbeReport;
use eyre::Result;

mod cli_driver;
mod runner;

pub use cli_driver::CliWizardDriver;
pub use runner::{Wizard, WizardOpts};

/// The five wizard steps, in execution order. The runner never skips
/// or reorders them; drivers can use this to render progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    DataRoot,
    Pdfium,
    Ollama,
    Smoke,
    Finalize,
}

/// Inputs the runner hands to the driver for step 1. `portable` is the
/// candidate from `bookrack_config::portable_data_dir()`; `data_dir` is
/// the operator's `--data-dir` value, if any. `force` mirrors
/// [`WizardOpts::force`] so the driver can refuse a populated root
/// before rendering its choice; the runner re-validates the returned
/// path with the same predicate either way.
pub struct DataRootHint {
    pub portable: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub non_interactive: bool,
    pub force: bool,
}

/// Result of step 2: the PDFium library search. Warn-only; EPUB and
/// TXT ingest still work without the library.
pub struct PdfiumReport {
    /// Platform filename of the dynamic library.
    pub filename: &'static str,
    /// Full path of the library, when the search found it.
    pub found: Option<PathBuf>,
    /// Every directory the search checked, in order.
    pub probed: Vec<PathBuf>,
    /// Whether a pinned binary is published for this platform, i.e.
    /// whether offering a download is meaningful.
    pub installable: bool,
}

/// Driver's answer to the PDFium step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfiumChoice {
    /// Proceed without the library; PDF ingest stays unavailable until
    /// it appears.
    Continue,
    /// Download the pinned binary into the managed directory. Only
    /// meaningful when the report says `installable` and nothing was
    /// found.
    Install,
}

/// Outcome of the wizard-initiated PDFium install, for presentation.
pub enum PdfiumInstallOutcome {
    /// The library now sits at this path.
    Installed(PathBuf),
    /// The download or unpack failed; the wizard continues without it.
    Failed(String),
}

/// Step 3 inputs. `report` is the existing embed probe; `embed_model`
/// is the name the runner intends to record into `config.toml`.
pub struct OllamaStep<'a> {
    pub url: &'a str,
    pub embed_model: &'a str,
    pub report: &'a EmbedProbeReport,
}

/// Result of step 4. `Skipped` carries the `--no-smoke` decision so
/// the driver can render the right line.
pub enum SmokeOutcome {
    Skipped,
    Ran(SmokeReport),
}

pub struct SmokeReport {
    pub chunks_written: usize,
    pub hits: usize,
    pub marker_query: &'static str,
}

/// Result of step 5. `registry` is `None` when
/// `default_registry_path()` returned `None` and the wizard left a
/// `BOOKRACK_DATA_DIR` hint to the driver instead.
pub struct FinalizeSummary {
    pub data_root: PathBuf,
    pub config_path: PathBuf,
    pub config_kept: bool,
    pub manifest_path: PathBuf,
    pub manifest_kept: bool,
    pub registry: Option<PathBuf>,
}

#[async_trait::async_trait]
pub trait WizardDriver: Send + Sync {
    /// Step 1: return the absolute data root the rest of the wizard
    /// will operate on. Driver owns prompts, defaults, and refusal.
    async fn step_data_root(&self, hint: DataRootHint) -> Result<PathBuf>;

    /// Step 2: present the library search finding and decide whether
    /// to install the pinned binary. Warn-only — driver must not abort
    /// on a missing library, and must answer
    /// [`PdfiumChoice::Continue`] when the library was found or no
    /// pinned binary exists for the platform.
    async fn step_pdfium(&self, report: &PdfiumReport) -> Result<PdfiumChoice>;

    /// Step 2b: present the install outcome. Only called when step 2
    /// answered [`PdfiumChoice::Install`]. Warn-only — a failed
    /// install leaves PDF ingest unavailable, nothing worse.
    async fn step_pdfium_install(&self, outcome: &PdfiumInstallOutcome) -> Result<()>;

    /// Step 3: present the Ollama probe. Driver decides whether to
    /// abort on unreachable / missing model.
    async fn step_ollama(&self, step: &OllamaStep<'_>) -> Result<()>;

    /// Step 4: present the smoke outcome. Driver decides abort vs
    /// continue if `outcome.hits == 0`.
    async fn step_smoke(&self, outcome: &SmokeOutcome) -> Result<()>;

    /// Step 5: present the finalize summary. Runner has already
    /// written the config + registry; this is the closing line.
    async fn step_finalize(&self, summary: &FinalizeSummary) -> Result<()>;
}
