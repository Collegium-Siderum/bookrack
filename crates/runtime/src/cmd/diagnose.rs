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
    if let Some(warning) = scrub_gap_warning(&report.scrub_gaps) {
        eprint!("{warning}");
    }
    Ok(())
}

/// Render the operator warning for a partially redacted bundle, or
/// `None` when every redaction had its input.
///
/// The bundle is the file an operator attaches to a bug report, so a
/// gap has to be visible before it is sent, not discoverable inside
/// the tarball afterwards.
fn scrub_gap_warning(gaps: &[&str]) -> Option<String> {
    if !gaps.contains(&bookrack_diagnose::SCRUB_GAP_HOME_DIR) {
        return None;
    }
    Some(
        "warning: cannot determine the home directory; home paths are not redacted\n  \
         Set HOME and run again, or review the bundle before sending it.\n"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_home_dir_gap_produces_a_warning_naming_the_unredacted_paths() {
        let warning = scrub_gap_warning(&[bookrack_diagnose::SCRUB_GAP_HOME_DIR])
            .expect("home_dir gap warns");
        assert!(warning.starts_with("warning: cannot determine the home directory"));
        assert!(warning.contains("home paths are not redacted"));
        // The remedy belongs to the hint line, not the summary line.
        let summary = warning.lines().next().unwrap();
        assert!(!summary.contains("Set HOME"));
        assert!(warning.contains("Set HOME and run again"));
    }

    #[test]
    fn full_coverage_and_unrelated_gaps_stay_silent() {
        assert!(scrub_gap_warning(&[]).is_none());
        assert!(scrub_gap_warning(&["some_other_gap"]).is_none());
    }
}
