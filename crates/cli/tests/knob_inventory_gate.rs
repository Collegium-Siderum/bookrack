// SPDX-License-Identifier: Apache-2.0

//! The gate that binds `.env.example` to the code.
//!
//! `.env.example` is the authoritative, commented list of every
//! environment knob, and until now nothing checked it against the
//! binary: it stayed correct because someone remembered. A knob added
//! without a stanza there was undocumented, and a stanza left behind by
//! a removed knob was a lie, and neither showed up as a failure.
//!
//! Two rules, both mechanical:
//!
//!   1. the variables `bookrack config knobs` reports and the ones
//!      `.env.example` declares are the same set;
//!   2. every crate that reports knob origins is a contributor to the
//!      two surfaces that collect them.
//!
//! Rule 2 exists because rule 1 cannot see a crate nobody collects: an
//! unreached `knob_origins` reports nothing, so its knobs would be
//! absent from the inventory and rule 1 would demand they leave
//! `.env.example` too. That reads as "delete the documentation" when
//! the defect is a missing line in the collector.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bookrack_core::knob::Layer;
use eyre::Result;

/// The workspace root, from this crate's manifest directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<name> always has two ancestors")
        .to_path_buf()
}

/// The variables `.env.example` declares.
///
/// Only assignment lines count, commented-out ones included — a knob
/// normally left unset is still documented. Prose mentions do not: the
/// file names other variables inside its comments (`BOOKRACK_LOG` is
/// cited under `BOOKRACK_LOG_CONSOLE`), and counting those would let a
/// knob pass the gate on the strength of someone else's paragraph.
fn declared_in_env_example(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(|line| {
            let line = line.trim_start();
            line.strip_prefix('#').unwrap_or(line).trim_start()
        })
        .filter_map(|line| line.split_once('='))
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("BOOKRACK_") && !name.contains(char::is_whitespace))
        .map(str::to_string)
        .collect()
}

/// The variables the inventory names, from every knob's settable sites
/// and every native dependency's override.
fn named_by_the_inventory() -> BTreeSet<String> {
    let mut names: BTreeSet<String> = bookrack_cli::config_knobs::catalog()
        .iter()
        .flat_map(|row| row.chain.iter().map(|site| site.site.clone()))
        .filter(|site| site.starts_with("BOOKRACK_"))
        .collect();
    names.extend(
        bookrack_config::NATIVE_DEPENDENCY_KNOBS
            .iter()
            .map(|dep| dep.override_site.to_string()),
    );
    names
}

/// Every variable the binary reads is documented, and every stanza in
/// `.env.example` describes a variable the binary reads.
///
/// Both directions, because the two failures are different: the first
/// leaves an operator unable to discover a knob, the second sends them
/// to set one that does nothing.
#[test]
fn env_example_declares_exactly_the_knobs_the_inventory_names() -> Result<()> {
    let path = workspace_root().join(".env.example");
    let declared = declared_in_env_example(&std::fs::read_to_string(&path)?);
    let named = named_by_the_inventory();

    let undocumented: Vec<&String> = named.difference(&declared).collect();
    assert!(
        undocumented.is_empty(),
        "the binary reads variables {path:?} does not document: {undocumented:?}. \
         Add a stanza for each, with what it does and its default."
    );

    let stale: Vec<&String> = declared.difference(&named).collect();
    assert!(
        stale.is_empty(),
        "{path:?} documents variables the binary no longer reads: {stale:?}. \
         Remove each stanza, or restore the knob it describes."
    );

    Ok(())
}

/// Every key `bookrack libraries config` writes is a key the inventory
/// reports, and the reverse.
///
/// The `config.toml` counterpart of the `.env.example` rule above, and
/// the two failures mirror it: a writable key missing from the inventory
/// is a setting an operator can put in the file and never find again, and
/// a file rung with no writable key behind it points at a line the write
/// surface refuses to produce.
///
/// [`Layer::File`] names one file — the data root's `config.toml` — so
/// every rung on that layer is a key of that file and the two sets are
/// comparable without filtering.
#[test]
fn the_inventory_names_exactly_the_config_toml_keys_that_can_be_written() {
    let reported: BTreeSet<String> = bookrack_cli::config_knobs::catalog()
        .iter()
        .flat_map(|row| row.chain.iter())
        .filter(|site| site.layer == Layer::File)
        .map(|site| site.site.clone())
        .collect();
    let writable: BTreeSet<String> = bookrack_config::ROOT_CONFIG_KEYS
        .iter()
        .map(|key| key.to_string())
        .collect();

    let unreported: Vec<&String> = writable.difference(&reported).collect();
    assert!(
        unreported.is_empty(),
        "`libraries config` writes keys the inventory does not report: {unreported:?}. \
         Give each one a file rung in the resolver that owns it."
    );

    let unwritable: Vec<&String> = reported.difference(&writable).collect();
    assert!(
        unwritable.is_empty(),
        "the inventory reports file keys `libraries config` will not write: {unwritable:?}. \
         Add each to ROOT_CONFIG_KEYS, or drop the rung."
    );
}

/// Both entry points of every knob-reporting crate are named by a
/// collector.
///
/// A source scan rather than a call, because the failure is a crate
/// nobody calls: an unreached `pub fn knob_origins` contributes no rows,
/// so no amount of calling the collectors can reveal it.
///
/// The `Contributor` pair means one entry in `CONTRIBUTORS` serves both
/// surfaces, so most crates are checked against `config_effective.rs`
/// alone. `bookrack-config` is the exception: its report takes the
/// resolved `Config` no other contributor takes, so each surface calls
/// it directly, and both of those calls are checked here — dropping one
/// would leave a surface silently short of six knobs.
#[test]
fn every_knob_reporting_crate_is_named_by_a_collector() -> Result<()> {
    let root = workspace_root();
    let report = std::fs::read_to_string(root.join("crates/cli/src/config_effective.rs"))?;
    let inventory = std::fs::read_to_string(root.join("crates/cli/src/config_knobs.rs"))?;

    let mut missing = Vec::new();
    let mut checked = 0usize;

    for entry in std::fs::read_dir(root.join("crates"))? {
        let dir = entry?.path();
        let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if !reports_knob_origins(&dir.join("src"))? {
            continue;
        }
        checked += 1;

        // Where each collector names this crate. `cli` reports its own
        // knob from a module rather than through a crate name, and
        // `config` is called directly by each surface rather than
        // through `CONTRIBUTORS`, so its catalog call is the one that
        // lives in the inventory file.
        let (path, catalog_in) = match name.as_str() {
            "config" => (
                "bookrack_config::".to_string(),
                ("config_knobs.rs", &inventory),
            ),
            "cli" => (
                "crate::render::confirm::".to_string(),
                ("config_effective.rs", &report),
            ),
            other => (
                format!("bookrack_{}::", other.replace('-', "_")),
                ("config_effective.rs", &report),
            ),
        };

        // Matched per line rather than as one token, because a crate
        // may expose its reporter from a module
        // (`bookrack_extract::pdfium_gate::knob_origins`) and a rule
        // keyed to the crate path alone would then miss it.
        for (fname, (which, source)) in [
            ("knob_origins", ("config_effective.rs", &report)),
            ("knob_catalog", catalog_in),
        ] {
            let named = source
                .lines()
                .any(|line| line.contains(&path) && line.contains(fname));
            if !named {
                missing.push(format!("{which} names no {path}… {fname}"));
            }
        }
    }

    assert!(
        checked >= 5,
        "the scan found only {checked} knob-reporting crates, so it is no \
         longer looking where they live"
    );
    assert!(
        missing.is_empty(),
        "a crate reports knobs that no collector reaches, so its knobs are \
         missing from a surface claiming to list them all: {missing:?}"
    );

    Ok(())
}

/// Whether any file under `dir` exposes `pub fn knob_origins`.
fn reports_knob_origins(dir: &Path) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if reports_knob_origins(&path)? {
                return Ok(true);
            }
        } else if path.extension().is_some_and(|e| e == "rs")
            && std::fs::read_to_string(&path)?.contains("pub fn knob_origins")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod parsing {
    use super::declared_in_env_example;

    /// A variable named only in prose is not a declaration. Without
    /// this the gate would accept a knob that someone mentioned in
    /// another knob's paragraph as documented in its own right.
    #[test]
    fn a_prose_mention_is_not_a_declaration() {
        let text = "\
# Console verbosity, using the same grammar as BOOKRACK_LOG.
BOOKRACK_LOG_CONSOLE=error
";
        let declared = declared_in_env_example(text);

        assert!(declared.contains("BOOKRACK_LOG_CONSOLE"));
        assert!(
            !declared.contains("BOOKRACK_LOG"),
            "a mention inside a comment was counted as a declaration: {declared:?}"
        );
    }

    /// A knob normally left unset is declared commented out, and still
    /// counts: the stanza above it is its documentation.
    #[test]
    fn a_commented_out_assignment_still_declares() {
        let declared = declared_in_env_example("#BOOKRACK_VECTORS_NPROBES=\n");

        assert!(
            declared.contains("BOOKRACK_VECTORS_NPROBES"),
            "{declared:?}"
        );
    }
}
