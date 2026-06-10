// SPDX-License-Identifier: Apache-2.0

//! `bookrack init`: the interactive install wizard.
//!
//! Thin shim over `bookrack_runtime::wizard`: the five-step state
//! machine and its on-disk side effects live in the runtime crate,
//! driven here through the terminal `CliWizardDriver`.
//!
//! The wizard dispatches before `Config::resolve` runs — the resolver
//! errors out on an unconfigured install, which is the very state init
//! is meant to fix.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_runtime::wizard::{CliWizardDriver, Wizard, WizardOpts};

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
    let opts = WizardOpts {
        force: args.force,
        no_smoke: args.no_smoke,
        non_interactive: args.non_interactive,
        data_dir: args.data_dir,
    };
    let driver = CliWizardDriver {
        non_interactive: opts.non_interactive,
    };
    Wizard::run(&driver, opts).await
}
