// SPDX-License-Identifier: Apache-2.0

//! `bookrack diagnose` — assemble a crash bundle.

use std::path::PathBuf;

use bookrack_config::Config;
use eyre::{Context, Result};

pub fn run(cfg: &Config, out: Option<PathBuf>, days: u32, no_scrub: bool) -> Result<()> {
    let opts = bookrack_diagnose::Options {
        days,
        scrub: !no_scrub,
        out,
        now: None,
    };
    let report = bookrack_diagnose::collect(cfg, &opts).context("collect diagnose bundle")?;
    println!("diagnose bundle: {}", report.out_path.display());
    println!("  files: {}", report.files);
    println!("  scrubbed: {}", report.scrubbed);
    Ok(())
}
