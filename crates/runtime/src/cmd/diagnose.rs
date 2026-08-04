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
    let mut warning = String::new();
    if gaps.contains(&bookrack_diagnose::SCRUB_GAP_HOME_DIR) {
        warning.push_str(
            "warning: cannot determine the home directory; home paths are not redacted\n  \
             Set HOME and run again, or review the bundle before sending it.\n",
        );
    }
    if gaps.contains(&bookrack_diagnose::SCRUB_GAP_HOME_DIR_UNVERIFIED) {
        warning.push_str(
            "warning: HOME names a directory that does not exist; home paths outside \
             the generic user roots are not redacted\n  \
             The redaction folded the path HOME gave, which nothing on this machine \
             lives under. Correct HOME and run again, or review the bundle before \
             sending it.\n",
        );
    }
    (!warning.is_empty()).then_some(warning)
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

    /// The redaction ran here, against a prefix the host does not
    /// have, so the warning must not repeat the "cannot determine"
    /// wording: the operator's next step is different, and a bundle
    /// that claims full coverage while carrying home paths is worse
    /// than one that admits it found no home at all.
    #[test]
    fn an_unverified_home_warns_in_its_own_words() {
        let warning = scrub_gap_warning(&[bookrack_diagnose::SCRUB_GAP_HOME_DIR_UNVERIFIED])
            .expect("an unverified home warns");
        let summary = warning.lines().next().expect("a summary line");
        assert!(
            summary.contains("HOME") && summary.contains("does not exist"),
            "the summary must state which input is doubted: {summary:?}"
        );
        assert!(
            !summary.contains("cannot determine the home directory"),
            "a home that resolved is not a home that could not be found: {summary:?}"
        );
        assert!(
            warning.contains("home paths"),
            "the consequence for the bundle is the point: {warning:?}"
        );
    }

    /// Both shortfalls cannot occur together — a home is either
    /// unresolved or resolved from somewhere — but the renderer takes
    /// a list, and a list it silently drops half of is the failure
    /// mode worth pinning.
    #[test]
    fn every_gap_in_the_list_reaches_the_operator() {
        let warning = scrub_gap_warning(&[
            bookrack_diagnose::SCRUB_GAP_HOME_DIR,
            bookrack_diagnose::SCRUB_GAP_HOME_DIR_UNVERIFIED,
        ])
        .expect("two gaps warn");
        assert!(warning.contains("cannot determine the home directory"));
        assert!(warning.contains("does not exist"));
    }

    #[test]
    fn full_coverage_and_unrelated_gaps_stay_silent() {
        assert!(scrub_gap_warning(&[]).is_none());
        assert!(scrub_gap_warning(&["some_other_gap"]).is_none());
    }
}
