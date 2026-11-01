// SPDX-License-Identifier: Apache-2.0

//! The gate that binds every route to a data root to a judgement about
//! the root's identity.
//!
//! A registered library's root is that library's only while its
//! manifest records the uuid the registry caches. The daemon judges
//! this when it mounts a library, and a command that writes from the
//! CLI process judges it through `identity_check::refuse_offline_write`.
//! A write path that takes a root and skips the judgement writes into
//! an empty mount point, or into another library, and says nothing.
//!
//! So every call that hands the code a data root, or the registry
//! entries that name one, carries a `// root identity:` marker on the
//! line directly above it, naming one of four classes:
//!
//! - `gated`: the path judges the root before it writes; the note says
//!   where when that is not in the same function;
//! - `read-only`: the path writes nothing to the root;
//! - `mount-checked`: a daemon path, which judges the root at mount;
//! - `registry`: the call reads registry entries and touches no root.
//!
//! The marker is a person's judgement. The gate checks only that it is
//! there and spelled from the closed set; whether a `gated` path does
//! refuse is pinned by the offline write tests, which run the binary
//! against each kind of foreign root.

use std::path::Path;

use eyre::Result;

use crate::source_scan::{
    marker_above, member_crates, rust_sources, test_module_start, workspace_root,
};

/// The marker that records the judgement.
const MARKER: &str = "// root identity:";

/// The classes a marker may name.
const CLASSES: &[&str] = &["gated", "read-only", "mount-checked", "registry"];

/// The calls that hand the code a data root or the registry entries
/// that name one, as (qualifier, function) pairs, joined into the text
/// they are matched by when the scan runs.
const CALLS: &[(&str, &str)] = &[
    ("Config", "resolve"),
    ("Config", "load"),
    ("", "find_library"),
    ("", "list_libraries"),
    ("", "list_libraries_with_default"),
    ("", "list_libraries_at"),
];

/// The text a call is matched by: `<qualifier>::<function>(`, or a bare
/// `<function>(` for an unqualified one.
fn needle(qualifier: &str, function: &str) -> String {
    if qualifier.is_empty() {
        format!("{function}(")
    } else {
        format!("{qualifier}::{function}(")
    }
}

/// The calls on one line of source, by needle.
///
/// A match counts only at a word boundary, so a method of the same name
/// on another type, or a longer function name ending in a listed one,
/// is not a call. The `fn` line defining a listed function is not a
/// call either, and a comment line holds none.
fn calls_on(line: &str, needles: &[String]) -> Vec<String> {
    if line.trim_start().starts_with("//") {
        return Vec::new();
    }
    let mut calls = Vec::new();
    for needle in needles {
        let mut from = 0;
        while let Some(pos) = line[from..].find(needle.as_str()) {
            let at = from + pos;
            from = at + needle.len();
            let before = &line[..at];
            let bounded = before
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            let defines = before.split_whitespace().next_back() == Some("fn");
            if bounded && !defines {
                calls.push(needle.clone());
            }
        }
    }
    calls
}

/// One call found in the source, with the marker above it.
#[derive(Debug)]
struct Site {
    /// `crates/<crate>/src/…:<line>`, for a failure message that can be
    /// jumped to.
    site: String,
    /// The text the call was matched by.
    call: String,
    /// The marker's payload, or `None` when the call carries none.
    marker: Option<String>,
}

/// Every listed call in every member crate, outside test modules.
fn call_sites(root: &Path) -> Result<Vec<Site>> {
    let needles: Vec<String> = CALLS.iter().map(|(q, f)| needle(q, f)).collect();
    let mut sites = Vec::new();
    for name in member_crates(root)? {
        for path in rust_sources(&root.join("crates").join(&name))? {
            let text = std::fs::read_to_string(&path)?;
            let all: Vec<&str> = text.lines().collect();
            let lines = &all[..test_module_start(&all)];
            let shown = path.strip_prefix(root).unwrap_or(&path).display();
            for (idx, line) in lines.iter().enumerate() {
                for call in calls_on(line, &needles) {
                    sites.push(Site {
                        site: format!("{shown}:{}", idx + 1),
                        call,
                        marker: marker_above(lines, idx, MARKER),
                    });
                }
            }
        }
    }
    sites.sort_by(|a, b| a.site.cmp(&b.site));
    Ok(sites)
}

/// Every route to a data root says what guards a write through it.
///
/// The direction that catches an addition: a new call is either judged,
/// read-only, the daemon's, or the registry's, and this is the point at
/// which that has to be answered.
#[test]
fn every_route_to_a_data_root_carries_a_marker() -> Result<()> {
    let sites = call_sites(&workspace_root())?;
    let unmarked: Vec<String> = sites
        .iter()
        .filter(|s| s.marker.is_none())
        .map(|s| format!("{} ({})", s.site, s.call))
        .collect();
    assert!(
        unmarked.is_empty(),
        "these calls carry no `{MARKER}` marker, so nothing says whether a write through \
         them is judged against the root's identity: {unmarked:?}. Write \
         `{MARKER} <class>[; <note>]` on the line directly above the call, with <class> one \
         of {CLASSES:?}."
    );
    assert!(
        sites.len() > 20,
        "the scan found only {} calls in the workspace, so it is no longer looking where \
         they live",
        sites.len()
    );
    Ok(())
}

/// A marker names a class from the closed set.
#[test]
fn every_marker_names_a_known_class() -> Result<()> {
    let sites = call_sites(&workspace_root())?;
    let unknown: Vec<String> = sites
        .iter()
        .filter_map(|s| {
            let payload = s.marker.as_deref()?;
            let class = payload.split(';').next().unwrap_or_default().trim();
            (!CLASSES.contains(&class)).then(|| format!("{}: {payload}", s.site))
        })
        .collect();
    assert!(
        unknown.is_empty(),
        "these `{MARKER}` markers name no known class: {unknown:?}. The class is the text \
         before the first `;`, one of {CLASSES:?}."
    );
    Ok(())
}

/// What the scan takes for a call, and what only looks like one.
#[test]
fn the_scan_counts_calls_and_nothing_that_only_looks_like_one() {
    let resolve = needle("Config", "resolve");
    let listing = needle("", "list_libraries");
    let needles = vec![resolve.clone(), listing.clone()];
    let calls = |line: &str| calls_on(line, &needles);

    assert_eq!(
        calls(&format!("    let cfg = {resolve}sel)?;")),
        [resolve.as_str()]
    );
    assert_eq!(
        calls(&format!("    bookrack_config::{resolve}sel)")),
        [resolve.as_str()]
    );
    assert_eq!(calls("    let e = list_libraries()?;"), [listing.as_str()]);
    assert!(calls(&format!("    // the {resolve}sel) above")).is_empty());
    assert!(calls(&format!("    /// [{resolve}sel)]")).is_empty());
    assert!(calls(&format!("    let e = Embed{resolve}c);")).is_empty());
    assert!(calls("    let e = list_libraries_at(&p)?;").is_empty());
    assert!(calls("pub fn list_libraries() -> Result<()> {").is_empty());
}
