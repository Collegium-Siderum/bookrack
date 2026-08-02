// SPDX-License-Identifier: Apache-2.0

//! The gate that binds `bookrack config fixed` to the constants.
//!
//! A compiled-in value is discoverable only if the inventory names it,
//! and the inventory is trustworthy only if nothing can be left out of
//! it quietly. So the rule runs over the source rather than over the
//! registrations: every numeric constant in a covered crate carries a
//! `// setting:` marker naming either the key it is registered under or
//! the reason it is not a setting at all, and the two sets are compared
//! in both directions.
//!
//! A naming rule was the obvious cheaper alternative — scan for
//! `DEFAULT_*` / `MAX_*` / `*_TIMEOUT` and demand those be registered —
//! and it fails on this workspace: it reaches about half the tunable
//! constants and misses whole families (`RERANK_MAX_RETRIES`,
//! `NOFILE_TARGET`, `L2_ABSOLUTE_FLOOR`, `TAIL_DEFAULT`, `BACKUP_KEEP`).
//! A gate that silently covers half its subject is worse than none: it
//! reads as coverage. Hence the exhaustive form, where the judgement is
//! a line a person writes and the gate only checks that the line exists.
//!
//! The rule runs over every member crate. It was rolled out behind a
//! list of the crates already accounted for; that list is gone, which
//! is what makes a crate added later subject to the rule on the day it
//! appears rather than on the day someone remembers to add it.
//!
//! One relaxation: a file whose constants are all the same kind of
//! not-a-setting states that once, above its first constant, instead of
//! repeating one sentence down a column — the JSON-RPC code table is
//! the case it exists for. The cost is real and worth naming: a tunable
//! later added to such a file inherits the exemption silently. It is
//! bounded by what the file-scoped form cannot do — it cannot name a
//! key, so nothing reaches the inventory through it, and a constant
//! that carries its own marker is still held to it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use eyre::Result;

/// Constant types the rule applies to.
///
/// Numbers and durations, because those are what a tuning or debugging
/// question is asked about. A `&str` constant is a name, a SQL
/// fragment, or a key, and none of those has a value an operator would
/// go looking for.
const TUNABLE_TYPES: &[&str] = &[
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "f32",
    "f64",
    "Duration",
    "std::time::Duration",
];

/// The marker that ties a constant to its registration.
const MARKER: &str = "// setting:";

/// The marker payload that opts a constant out of the inventory.
const NOT_A_SETTING: &str = "internal";

/// Every member crate, by directory name.
///
/// Read off the filesystem rather than held as a list: a list is a
/// second place to remember, and the failure it produces — a crate
/// nobody added — is silent.
fn member_crates(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root.join("crates"))? {
        let path = entry?.path();
        if !path.join("src").is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// The workspace root, from this crate's manifest directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<name> always has two ancestors")
        .to_path_buf()
}

/// One numeric constant found in the source, with the marker above it.
#[derive(Debug)]
struct Found {
    /// `crates/<crate>/src/…:<line>`, for a failure message that can be
    /// jumped to.
    site: String,
    /// The constant's name.
    name: String,
    /// The `// setting:` payload, or `None` when the constant carries
    /// no marker.
    marker: Option<String>,
}

/// Every numeric constant a crate declares outside its test modules.
///
/// The scan stops where a file's test module starts: a constant a test
/// declares for its own fixture is not part of the build's
/// configuration, and demanding a marker on one would train the reflex
/// to write `internal` without thinking.
fn numeric_constants(crate_dir: &Path) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    let mut stack = vec![crate_dir.join("src")];
    while let Some(dir) = stack.pop() {
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            let all: Vec<&str> = text.lines().collect();
            let lines = &all[..test_module_start(&all)];
            let whole_file = file_scoped_marker(lines);
            for (idx, line) in lines.iter().enumerate() {
                let Some(name) = declares_numeric_constant(line) else {
                    continue;
                };
                found.push(Found {
                    site: format!("{}:{}", path.display(), idx + 1),
                    name,
                    marker: marker_above(lines, idx).or_else(|| whole_file.clone()),
                });
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Where a file's test module begins, or its length when it has none.
///
/// `#[cfg(test)]` alone does not mark it: the attribute also gates
/// imports a test-only derive needs, and cutting at the first one of
/// those truncates a file at its `use` block and hides every constant
/// below. Only the attribute standing on a `mod` starts the tests.
fn test_module_start(lines: &[&str]) -> usize {
    for (idx, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let opens_a_module = lines[idx + 1..]
            .iter()
            .find(|next| !next.trim().is_empty())
            .is_some_and(|next| {
                let next = next.trim();
                next.starts_with("mod ") || next.starts_with("pub mod ")
            });
        if opens_a_module {
            return idx;
        }
    }
    lines.len()
}

/// The constant's name when this line declares one of a tunable type.
fn declares_numeric_constant(line: &str) -> Option<String> {
    let line = line.trim_start();
    let line = line
        .strip_prefix("pub(crate) ")
        .or_else(|| line.strip_prefix("pub(super) "))
        .or_else(|| line.strip_prefix("pub "))
        .unwrap_or(line);
    let rest = line.strip_prefix("const ")?;
    let (name, rest) = rest.split_once(':')?;
    let name = name.trim();
    // `const _: T = ...` names nothing, so there is nothing to look up
    // and no marker to demand.
    if !name.chars().any(|c| c.is_ascii_uppercase())
        || !name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
    {
        return None;
    }
    let ty = rest.split('=').next()?.trim();
    TUNABLE_TYPES.contains(&ty).then(|| name.to_string())
}

/// The `// setting:` payload attached to the constant declared at
/// `idx`.
///
/// The marker sits on the line directly above the declaration,
/// attributes skipped — below the doc comment, which describes the
/// constant, rather than above it, which would put a machine-readable
/// line where a reader expects prose.
fn marker_above(lines: &[&str], idx: usize) -> Option<String> {
    let mut cursor = idx;
    while cursor > 0 {
        cursor -= 1;
        let line = lines[cursor].trim();
        if line.starts_with('#') {
            continue;
        }
        return line
            .strip_prefix(MARKER)
            .map(|payload| payload.trim().to_string());
    }
    None
}

/// The one opt-out a whole file may state on behalf of its constants.
///
/// Taken only from before the file's first constant, and only in the
/// opt-out form: a file-scoped marker naming a key would be a
/// registration with no constant behind it, which is the shape the
/// key-set comparison exists to reject.
fn file_scoped_marker(lines: &[&str]) -> Option<String> {
    for (idx, line) in lines.iter().enumerate() {
        if declares_numeric_constant(line).is_some() {
            return None;
        }
        let Some(payload) = line.trim().strip_prefix(MARKER) else {
            continue;
        };
        // A marker standing directly above a constant is that
        // constant's, whatever it precedes in the file.
        let attaches_to_a_constant = lines[idx + 1..]
            .iter()
            .find(|next| !next.trim().is_empty() && !next.trim().starts_with('#'))
            .is_some_and(|next| declares_numeric_constant(next).is_some());
        if attaches_to_a_constant {
            continue;
        }
        let payload = payload.trim();
        assert!(
            payload.starts_with(NOT_A_SETTING),
            "line {} states a file-scoped `{MARKER} {payload}`, but a whole file may \
             only opt out, never register: a key needs the one constant it belongs to",
            idx + 1
        );
        return Some(payload.to_string());
    }
    None
}

/// Every constant in the workspace says what it is.
///
/// The direction that catches an addition: a new numeric constant is
/// either a setting someone can find or a decision someone wrote down,
/// and this is the point at which that has to be answered.
#[test]
fn every_numeric_constant_carries_a_marker() -> Result<()> {
    let root = workspace_root();
    let mut unmarked = Vec::new();
    let mut blank_reasons = Vec::new();
    let mut scanned = 0usize;

    for name in member_crates(&root)? {
        let found = numeric_constants(&root.join("crates").join(&name))?;
        scanned += found.len();
        for constant in found {
            match &constant.marker {
                None => unmarked.push(format!("{} ({})", constant.site, constant.name)),
                Some(payload) if payload == NOT_A_SETTING => {
                    blank_reasons.push(format!("{} ({})", constant.site, constant.name));
                }
                Some(_) => {}
            }
        }
    }

    assert!(
        unmarked.is_empty(),
        "these constants carry no `{MARKER}` marker, so nothing says whether an \
         operator can find them: {unmarked:?}. Add `{MARKER} <dotted.key>` and \
         register it, or `{MARKER} {NOT_A_SETTING} -- <why not>`."
    );
    assert!(
        blank_reasons.is_empty(),
        "these constants opt out of the inventory without saying why: {blank_reasons:?}. \
         Write `{MARKER} {NOT_A_SETTING} -- <why not>`; the reason is the whole value \
         of the opt-out, because it is what a later reader checks against."
    );
    assert!(
        scanned > 100,
        "the scan found only {scanned} numeric constants in the workspace, so it is no \
         longer looking where they live"
    );
    Ok(())
}

/// The markers and the registrations name the same keys.
///
/// Both directions, because the two failures are different: a marker
/// with no registration is a value the inventory promises and does not
/// print, and a registration with no marker is a row whose constant
/// nothing ties it to — the one that survives the constant being
/// renamed, moved, or deleted.
#[test]
fn the_inventory_registers_exactly_the_keys_the_markers_name() -> Result<()> {
    let root = workspace_root();

    let mut marked: BTreeSet<String> = BTreeSet::new();
    for name in member_crates(&root)? {
        for constant in numeric_constants(&root.join("crates").join(&name))? {
            match constant.marker {
                Some(payload) if !payload.starts_with(NOT_A_SETTING) => {
                    marked.insert(payload);
                }
                _ => {}
            }
        }
    }

    let registered: BTreeSet<String> = bookrack_cli::config_fixed::catalog()
        .iter()
        .map(|setting| setting.key.to_string())
        .collect();

    let unregistered: Vec<&String> = marked.difference(&registered).collect();
    assert!(
        unregistered.is_empty(),
        "constants are marked with keys the inventory does not register: {unregistered:?}. \
         Add each to the crate's `fixed_settings!` block."
    );

    let unmarked: Vec<&String> = registered.difference(&marked).collect();
    assert!(
        unmarked.is_empty(),
        "the inventory registers keys no constant is marked with: {unmarked:?}. \
         Mark the constant with `{MARKER} <key>`, or drop the registration."
    );

    Ok(())
}

/// One key names one value.
///
/// The property that makes the inventory able to find a duplicate
/// rather than print it twice: the same concept given two homes in two
/// crates cannot claim one key, so registering the second is a failure
/// instead of a second row a reader has to notice is the same number.
#[test]
fn no_two_registrations_claim_the_same_key() {
    let mut homes: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for setting in bookrack_cli::config_fixed::catalog() {
        homes.entry(setting.key).or_default().push(format!(
            "{}::{}",
            setting.owner,
            (setting.value)()
        ));
    }

    let clashes: Vec<(&&str, &Vec<String>)> =
        homes.iter().filter(|(_, homes)| homes.len() > 1).collect();
    assert!(
        clashes.is_empty(),
        "one key is registered by more than one crate: {clashes:?}. \
         Two homes for one value is the drift this inventory exists to catch: \
         give one crate the constant and let the other use it."
    );
}

/// Every covered crate that has something to say reaches the surface
/// that collects it.
///
/// The counterpart of the knob gate's collector rule, and it exists for
/// the same reason: a registration no surface reads contributes
/// nothing, so the key-set comparison above would pass by finding both
/// sides empty. Keyed on the markers rather than on membership, because
/// a crate whose every constant is a version stamp registers nothing
/// and is complete exactly so.
#[test]
fn every_crate_that_marks_a_key_is_collected_by_the_inventory() -> Result<()> {
    let root = workspace_root();
    let owners: BTreeSet<&str> = bookrack_cli::config_fixed::catalog()
        .iter()
        .map(|setting| setting.owner)
        .collect();

    let mut uncollected = Vec::new();
    for name in member_crates(&root)? {
        let names_a_key = numeric_constants(&root.join("crates").join(&name))?
            .iter()
            .any(|c| {
                c.marker
                    .as_deref()
                    .is_some_and(|m| !m.starts_with(NOT_A_SETTING))
            });
        if names_a_key && !owners.contains(name.as_str()) {
            uncollected.push(name);
        }
    }
    assert!(
        uncollected.is_empty(),
        "these crates mark constants with keys but contribute nothing to the inventory: \
         {uncollected:?}. Add each crate's `FIXED_SETTINGS` to `config_fixed::REGISTRIES`."
    );

    let members = member_crates(&root)?;
    let strays: Vec<&&str> = owners
        .iter()
        .filter(|owner| !members.iter().any(|name| name == *owner))
        .collect();
    assert!(
        strays.is_empty(),
        "these registrations name an owner that is not a member crate: {strays:?}. \
         The owner is what ties a row back to the source the gate scans."
    );

    Ok(())
}

/// Every row can be printed.
///
/// `value` is a function, so an entry that renders nothing is a row the
/// table shows blank — the failure mode a stored string does not have
/// and this shape trades for its immunity to drift.
#[test]
fn every_registration_renders_a_value() {
    for setting in bookrack_cli::config_fixed::catalog() {
        let value = (setting.value)();
        assert!(
            !value.trim().is_empty(),
            "{} renders an empty value",
            setting.key
        );
        assert!(
            !setting.summary.trim().is_empty(),
            "{} carries no summary",
            setting.key
        );
        assert!(
            setting.key.contains('.'),
            "{} is not a dotted key, so it does not share the knob namespace",
            setting.key
        );
    }
}
