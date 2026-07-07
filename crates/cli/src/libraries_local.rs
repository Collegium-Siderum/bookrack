// SPDX-License-Identifier: Apache-2.0

//! `bookrack libraries detect` / `libraries scan` — the read-only,
//! daemon-free surface for asking whether a path is a bookrack data
//! root. Detection itself lives in `bookrack_config::detect`; this
//! module only resolves the CLI's arguments, renders the verdict, and
//! maps it onto an exit code.

use std::path::PathBuf;

use bookrack_config::{
    DetectError, DetectVerdict, ScanOutcome, Signal, detect_library, mounted_volumes,
    scan_for_libraries,
};
use eyre::{Report, Result};
use serde::Serialize;

use crate::error::BookrackCliError;
use crate::render::ctx;

/// Descent depth for a `scan <parent>`: probe the parent's immediate
/// subdirectories.
const PARENT_SCAN_DEPTH: u8 = 1;

/// Descent depth for `scan --volumes`: each mounted volume and one level
/// within it.
const VOLUMES_SCAN_DEPTH: u8 = 2;

/// A detect verdict paired with the path it describes, for `--json`. The
/// verdict flattens in, contributing its `verdict` tag and payload.
#[derive(Serialize)]
struct DetectRecord<'a> {
    path: String,
    #[serde(flatten)]
    verdict: &'a DetectVerdict,
}

/// `libraries detect <path>`: probe one path, render the verdict, and
/// exit 0 for confirmed/probable, 1 for not-a-library/unreadable, 2 for
/// a bad path argument.
pub fn detect(path: PathBuf) -> Result<()> {
    let verdict = detect_library(&path).map_err(|e: DetectError| {
        Report::new(BookrackCliError::LocalUserError {
            message: e.to_string(),
        })
    })?;

    if ctx().is_json() {
        let record = DetectRecord {
            path: path.display().to_string(),
            verdict: &verdict,
        };
        println!(
            "{}",
            serde_json::to_string(&record).expect("detect record serializes")
        );
    } else if !ctx().is_quiet() {
        print_verdict_human(&path, &verdict);
    }

    match verdict {
        DetectVerdict::Confirmed(_) | DetectVerdict::Probable { .. } => Ok(()),
        DetectVerdict::NotALibrary { .. } | DetectVerdict::Unreadable { .. } => {
            Err(Report::new(BookrackCliError::DetectNegative(path)))
        }
    }
}

/// `libraries scan [parent] [--volumes]`: walk the chosen roots, list
/// the data roots found, and always exit 0 — a scan that finds nothing
/// still completed. Argument exclusivity is enforced by clap; this
/// function trusts exactly one of `parent`/`volumes` to be set.
pub fn scan(parent: Option<PathBuf>, volumes: bool) -> Result<()> {
    let (roots, depth) = if volumes {
        (mounted_volumes(), VOLUMES_SCAN_DEPTH)
    } else {
        // clap's ArgGroup guarantees a parent when `--volumes` is off.
        (
            vec![parent.expect("clap requires a parent without --volumes")],
            PARENT_SCAN_DEPTH,
        )
    };
    let outcome = scan_for_libraries(&roots, depth);

    if ctx().is_json() {
        print_scan_json(&outcome);
    } else if !ctx().is_quiet() {
        print_scan_human(&outcome);
    }
    Ok(())
}

/// Render one detect verdict as a human-readable line, with an indented
/// detail line for the identity (confirmed) or the signals found.
fn print_verdict_human(path: &std::path::Path, verdict: &DetectVerdict) {
    let display = path.display();
    match verdict {
        DetectVerdict::Confirmed(m) => {
            println!("confirmed: {display}");
            println!("  name={} kind={} uuid={}", m.name, m.kind.as_str(), m.uuid);
        }
        DetectVerdict::Probable { signals } => {
            println!("probable: {display}");
            println!("  signals: {}", render_signals(signals));
        }
        DetectVerdict::Unreadable { reason } => {
            println!("unreadable: {display}");
            println!("  {reason}");
        }
        DetectVerdict::NotALibrary { signals } => {
            println!("not a library: {display}");
            if !signals.is_empty() {
                println!("  signals: {}", render_signals(signals));
            }
        }
    }
}

/// Join a signal list into a comma-separated list of on-disk names.
fn render_signals(signals: &[Signal]) -> String {
    signals
        .iter()
        .map(|s| s.filename())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render a scan outcome as a table of found roots plus a summary line
/// that always reports how many entries were skipped.
fn print_scan_human(outcome: &ScanOutcome) {
    for (path, verdict) in &outcome.found {
        let (tag, name) = match verdict {
            DetectVerdict::Confirmed(m) => ("confirmed", m.name.as_str()),
            DetectVerdict::Probable { .. } => ("probable", "-"),
            // scan_for_libraries only ever collects confirmed/probable.
            _ => ("?", "-"),
        };
        println!("{tag:<9}  {name:<20}  {}", path.display());
    }
    println!(
        "{} librar{} found, {} skipped",
        outcome.found.len(),
        if outcome.found.len() == 1 { "y" } else { "ies" },
        outcome.skipped
    );
}

/// Render a scan outcome as a JSON object: an array of `{path, verdict,
/// ...}` records and the skipped count.
fn print_scan_json(outcome: &ScanOutcome) {
    let found: Vec<DetectRecord> = outcome
        .found
        .iter()
        .map(|(path, verdict)| DetectRecord {
            path: path.display().to_string(),
            verdict,
        })
        .collect();
    let value = serde_json::json!({
        "found": found,
        "skipped": outcome.skipped,
    });
    println!(
        "{}",
        serde_json::to_string(&value).expect("scan serializes")
    );
}
