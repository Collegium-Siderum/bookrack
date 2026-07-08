// SPDX-License-Identifier: Apache-2.0

//! `bookrack libraries detect` / `libraries scan` — the read-only,
//! daemon-free surface for asking whether a path is a bookrack data
//! root. Detection itself lives in `bookrack_config::detect`; this
//! module only resolves the CLI's arguments, renders the verdict, and
//! maps it onto an exit code.

use std::path::{Path, PathBuf};

use bookrack_config::{
    AddOptions, AddOutcome, AddReport, ConfigError, DetectError, DetectVerdict, LibraryKind,
    LibraryManifest, LibraryOpError, RootConfigSetError, ScanOutcome, Signal, add_library,
    detect_library, find_library, load_root_config, mounted_volumes, read_root_config_text,
    registry_target_path, remove_library, render_manifest_toml, repoint_library,
    root_config_env_override, scan_for_libraries, set_root_config_values,
};
use eyre::{Report, Result};
use serde::Serialize;

use crate::error::BookrackCliError;
use crate::render::confirm::{ConfirmMode, confirm_destructive};
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

/// `libraries scan [parent] [--volumes] [--register]`: walk the chosen
/// roots, list the data roots found, and always exit 0 — a scan that
/// finds nothing still completed. Argument exclusivity is enforced by
/// clap; this function trusts exactly one of `parent`/`volumes` to be
/// set. With `--register`, every confirmed root is added; probable roots
/// are listed but never auto-registered.
pub fn scan(
    parent: Option<PathBuf>,
    volumes: bool,
    register: bool,
    kind: Option<LibraryKind>,
) -> Result<()> {
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

    if register {
        return scan_register(&outcome, kind);
    }

    if ctx().is_json() {
        print_scan_json(&outcome);
    } else if !ctx().is_quiet() {
        print_scan_human(&outcome);
    }
    Ok(())
}

/// Register every confirmed root a scan found, skipping probable ones
/// with a warning. A per-root registration failure (a name or uuid
/// clash) is reported and counted, never aborting the sweep. Always
/// exits 0: recovering what it can is the point.
fn scan_register(outcome: &ScanOutcome, kind: Option<LibraryKind>) -> Result<()> {
    let registry_path = registry_path()?;
    let mut registered = 0usize;
    let mut probable_skipped = 0usize;
    let mut clashed = 0usize;
    for (path, verdict) in &outcome.found {
        match verdict {
            DetectVerdict::Confirmed(_) => {
                // Confirmed roots carry a manifest, so `add_library`
                // never prompts; identity is recovered from it verbatim.
                match add_library(
                    &registry_path,
                    None,
                    path,
                    kind,
                    None,
                    AddOptions::default(),
                    |_manifest| Ok(true),
                ) {
                    Ok(AddOutcome::Registered(report)) => {
                        registered += 1;
                        if !ctx().is_quiet() {
                            println!("registered '{}' -> {}", report.key, path.display());
                        }
                    }
                    Ok(AddOutcome::KeyTaken { key, .. }) => {
                        clashed += 1;
                        eprintln!(
                            "warning: {} skipped: name '{key}' already registered; \
                             add it manually under an alias",
                            path.display()
                        );
                    }
                    Ok(AddOutcome::UuidClash { existing_key, .. }) => {
                        clashed += 1;
                        eprintln!(
                            "warning: {} skipped: identity already registered as \
                             '{existing_key}'",
                            path.display()
                        );
                    }
                    Ok(AddOutcome::Aborted) => {}
                    Err(err) => {
                        clashed += 1;
                        eprintln!("warning: {} skipped: {err}", path.display());
                    }
                }
            }
            DetectVerdict::Probable { .. } => {
                probable_skipped += 1;
                if !ctx().is_quiet() {
                    eprintln!(
                        "warning: {} is probable but has no manifest; register it with \
                         'bookrack libraries add <name> {}'",
                        path.display(),
                        path.display()
                    );
                }
            }
            // scan_for_libraries only ever collects confirmed/probable.
            DetectVerdict::Unreadable { .. } | DetectVerdict::NotALibrary { .. } => {}
        }
    }
    if !ctx().is_quiet() {
        println!(
            "{registered} registered, {probable_skipped} probable skipped, \
             {clashed} clash(es), {} unreadable",
            outcome.skipped
        );
    }
    Ok(())
}

/// `libraries add <name> <path>` and `libraries register <path>`: register
/// an existing data root, writing an identity manifest first when the
/// root has none. `key` is `Some` for `add` (and `register --name`),
/// `None` for a bare `register` (the name is derived).
pub fn add(
    key: Option<String>,
    path: PathBuf,
    kind: Option<LibraryKind>,
    description: Option<String>,
    new_uuid: bool,
    yes: bool,
) -> Result<()> {
    let registry_path = registry_path()?;
    let confirm = |manifest: &LibraryManifest| -> std::io::Result<bool> {
        if !yes {
            eprintln!(
                "{} has no identity manifest; the following will be written:\n{}",
                path.display(),
                render_manifest_toml(manifest)
            );
        }
        confirm_destructive(
            "Write this manifest and register the library?",
            ConfirmMode::Soft,
            yes,
        )
    };
    let outcome = add_library(
        &registry_path,
        key.as_deref(),
        &path,
        kind,
        description,
        AddOptions { new_uuid },
        confirm,
    )
    .map_err(op_error)?;

    match outcome {
        AddOutcome::Registered(report) => {
            render_add_report(&report);
            Ok(())
        }
        AddOutcome::Aborted => {
            eprintln!("aborted; no changes written");
            Ok(())
        }
        AddOutcome::KeyTaken { key, existing_path } => {
            Err(Report::new(BookrackCliError::LocalUserError {
                message: format!(
                    "name '{key}' already registers {}; choose an alias with \
                     'bookrack libraries register {} --name <alias>'",
                    existing_path.display(),
                    path.display()
                ),
            }))
        }
        AddOutcome::UuidClash {
            uuid,
            existing_key,
            existing_path,
        } => resolve_uuid_clash(
            &registry_path,
            &path,
            &uuid,
            &existing_key,
            &existing_path,
            yes,
        ),
    }
}

/// Break a uuid clash. Interactively, offer to move the existing entry to
/// the new path; otherwise refuse and print the two exact commands so a
/// scripted caller can pick a resolution deliberately.
fn resolve_uuid_clash(
    registry_path: &Path,
    path: &Path,
    uuid: &str,
    existing_key: &str,
    existing_path: &Path,
    yes: bool,
) -> Result<()> {
    use std::io::IsTerminal;
    let interactive = !yes && std::io::stdin().is_terminal();
    if !interactive {
        return Err(Report::new(BookrackCliError::LocalUserError {
            message: format!(
                "uuid {uuid} is already registered as '{existing_key}'.\n\
                 to move it (same library, new path): bookrack libraries add {existing_key} {}\n\
                 to register a copy (new identity):   re-run with --new-uuid",
                path.display()
            ),
        }));
    }
    eprintln!(
        "uuid {uuid} is already registered as '{existing_key}' at {}.",
        existing_path.display()
    );
    eprintln!("  move: point '{existing_key}' at {}", path.display());
    eprintln!("  copy: re-run with --new-uuid to register a new identity");
    let move_it = confirm_destructive(
        "Enter 'move' to repoint the existing entry, anything else to abort:",
        ConfirmMode::Hard { token: "move" },
        false,
    )
    .map_err(|e| eyre::eyre!("read clash resolution: {e}"))?;
    if move_it {
        repoint_library(registry_path, existing_key, path).map_err(config_error)?;
        if !ctx().is_quiet() {
            println!("moved '{existing_key}' -> {}", path.display());
        }
        Ok(())
    } else {
        Err(Report::new(BookrackCliError::LocalUserError {
            message: format!(
                "not registered; to register a copy re-run: \
                 bookrack libraries register {} --new-uuid",
                path.display()
            ),
        }))
    }
}

/// `libraries remove <name> [--purge]`: forget a registry entry, and with
/// `--purge` delete its data root behind a detect gate and a typed
/// confirmation.
pub fn remove(name: String, purge: bool, yes: bool) -> Result<()> {
    let registry_path = registry_path()?;

    if purge {
        // Read the entry before removing it: the detect gate refuses to
        // delete a path that no longer looks like a data root, so an
        // entry pointing at the wrong directory cannot delete it.
        let entry = find_library(&registry_path, &name)
            .map_err(config_error)?
            .ok_or_else(|| {
                Report::new(BookrackCliError::LocalUserError {
                    message: format!("no library named '{name}' in the registry"),
                })
            })?;
        gate_purge_target(&entry.data_dir)?;
        let prompt = format!(
            "This deletes {} for good. Type the library name '{name}' to confirm:",
            entry.data_dir.display()
        );
        if !confirm_destructive(&prompt, ConfirmMode::Hard { token: &name }, yes)
            .map_err(|e| eyre::eyre!("read purge confirmation: {e}"))?
        {
            eprintln!("aborted; nothing removed");
            return Ok(());
        }
        remove_library(&registry_path, &name).map_err(config_error)?;
        std::fs::remove_dir_all(&entry.data_dir)
            .map_err(|e| eyre::eyre!("purge {}: {e}", entry.data_dir.display()))?;
        if !ctx().is_quiet() {
            println!("removed '{name}' and purged {}", entry.data_dir.display());
        }
        return Ok(());
    }

    let report = remove_library(&registry_path, &name).map_err(config_error)?;
    if !ctx().is_quiet() {
        println!(
            "removed '{name}'; data at {} kept",
            report.data_dir.display()
        );
        if report.default_cleared {
            println!("  default cleared; set a new one with 'bookrack libraries default <name>'");
        }
    }
    Ok(())
}

/// `libraries config <name> [KEY=VALUE ...] [--unset KEY]`: resolve the
/// library's data root from the registry, then read or edit its
/// `config.toml`. With no sets and no unsets, print the file; otherwise
/// apply the edits in place, preserving comments.
pub fn config(name: String, sets: Vec<(String, String)>, unset: Vec<String>) -> Result<()> {
    let registry_path = registry_path()?;
    let entry = find_library(&registry_path, &name)
        .map_err(config_error)?
        .ok_or_else(|| {
            Report::new(BookrackCliError::LocalUserError {
                message: format!("no library named '{name}' in the registry"),
            })
        })?;
    let data_dir = entry.data_dir;

    if sets.is_empty() && unset.is_empty() {
        return print_root_config(&name, &data_dir);
    }

    set_root_config_values(&data_dir, &sets, &unset).map_err(root_config_set_error)?;
    render_config_write(&name, &data_dir, &sets, &unset);
    Ok(())
}

/// Dump a library's `config.toml`: the parsed [`RootConfig`] for `--json`,
/// the raw file text (comments and all) for a human reader.
fn print_root_config(name: &str, data_dir: &Path) -> Result<()> {
    if ctx().is_json() {
        let cfg = load_root_config(data_dir).map_err(config_error)?;
        println!(
            "{}",
            serde_json::to_string(&cfg).expect("root config serializes")
        );
        return Ok(());
    }
    if ctx().is_quiet() {
        return Ok(());
    }
    let text = read_root_config_text(data_dir).map_err(config_error)?;
    if text.trim().is_empty() {
        println!(
            "'{name}' has no config.toml at {}",
            data_dir.join(bookrack_config::ROOT_CONFIG_NAME).display()
        );
    } else {
        print!("{text}");
    }
    Ok(())
}

/// Report a successful edit: the keys set and unset, plus the advisory
/// notes an operator needs — an embed-model change implies re-ingestion,
/// a set env var shadows the file, and the change only reaches a running
/// daemon on restart.
fn render_config_write(name: &str, data_dir: &Path, sets: &[(String, String)], unset: &[String]) {
    if ctx().is_json() {
        let value = serde_json::json!({
            "ok": true,
            "name": name,
            "data_dir": data_dir.display().to_string(),
            "set": sets.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
            "unset": unset,
        });
        println!("{value}");
    } else if !ctx().is_quiet() {
        for (key, value) in sets {
            println!("set {key} = {value:?}");
        }
        for key in unset {
            println!("unset {key}");
        }
    }

    if ctx().is_quiet() {
        return;
    }
    if sets.iter().any(|(key, _)| key == "embed_model") {
        eprintln!(
            "warning: changing embed_model requires re-ingestion; see 'bookrack vectors reset'"
        );
    }
    for (key, _) in sets {
        if let Some(env) = root_config_env_override(key)
            && std::env::var_os(env).is_some_and(|v| !v.is_empty())
        {
            eprintln!("note: {env} is set and overrides this value");
        }
    }
    eprintln!("note: restart the daemon (or re-run 'bookrack run') to apply");
}

/// Map a [`RootConfigSetError`] to the right exit code: an operator-input
/// fault (unknown key, invalid value, hand-corrupted file) is a user
/// error (exit 2); an I/O failure keeps the generic internal-error path.
fn root_config_set_error(err: RootConfigSetError) -> Report {
    match &err {
        RootConfigSetError::UnknownKey { .. }
        | RootConfigSetError::InvalidValue { .. }
        | RootConfigSetError::Malformed { .. } => Report::new(BookrackCliError::LocalUserError {
            message: err.to_string(),
        }),
        RootConfigSetError::Io(_) | RootConfigSetError::Write { .. } => Report::new(err),
    }
}

/// The detect gate for `remove --purge`: the target must look like a
/// data root (confirmed or probable) before its bytes are deleted, so an
/// entry that points at an unrelated directory cannot destroy it.
fn gate_purge_target(data_dir: &Path) -> Result<()> {
    match detect_library(data_dir) {
        Ok(DetectVerdict::Confirmed(_) | DetectVerdict::Probable { .. }) => Ok(()),
        _ => Err(Report::new(BookrackCliError::LocalUserError {
            message: format!(
                "refusing to purge {}: it is not a confirmed or probable data root",
                data_dir.display()
            ),
        })),
    }
}

/// Render a successful registration, plus a read-only warning and a
/// became-default note where they apply.
fn render_add_report(report: &AddReport) {
    if ctx().is_json() {
        let value = serde_json::json!({
            "key": report.key,
            "data_dir": report.data_dir.display().to_string(),
            "uuid": report.uuid,
            "wrote_manifest": report.wrote_manifest,
            "read_only": report.read_only_degraded,
            "default": report.became_default,
        });
        println!("{value}");
        return;
    }
    if ctx().is_quiet() {
        return;
    }
    if report.read_only_degraded {
        eprintln!("warning: read-only root, manifest not written; entry has no cached uuid");
    }
    let uuid = report
        .uuid
        .as_deref()
        .map(short_uuid)
        .unwrap_or_else(|| "-".to_string());
    println!(
        "registered '{}' -> {} (uuid {uuid})",
        report.key,
        report.data_dir.display()
    );
    if report.became_default {
        println!("  set as the default library");
    }
}

/// The first segment of a uuid, for a compact display.
fn short_uuid(uuid: &str) -> String {
    uuid.split('-').next().unwrap_or(uuid).to_string()
}

/// Resolve the registry file the offline write verbs edit, the same way
/// the daemon's fork helper does.
fn registry_path() -> Result<PathBuf> {
    registry_target_path().ok_or_else(|| {
        eyre::eyre!(
            "no registry location: set BOOKRACK_REGISTRY=<path> or ensure the platform \
             config directory is available"
        )
    })
}

/// Map a [`LibraryOpError`] to a report with the right exit code: an
/// operator-input fault (bad target, unreadable identity, unknown name)
/// becomes a user error; a registry or manifest I/O failure keeps the
/// generic (internal-error) path.
fn op_error(err: LibraryOpError) -> Report {
    match &err {
        LibraryOpError::BadTarget(_)
        | LibraryOpError::UnreadableTarget { .. }
        | LibraryOpError::Registry(ConfigError::UnknownLibrary { .. }) => {
            Report::new(BookrackCliError::LocalUserError {
                message: err.to_string(),
            })
        }
        _ => Report::new(err),
    }
}

/// Map a bare [`ConfigError`] the same way: an unknown-library fault is
/// operator input (user-error exit), everything else is generic.
fn config_error(err: ConfigError) -> Report {
    match &err {
        ConfigError::UnknownLibrary { .. } => Report::new(BookrackCliError::LocalUserError {
            message: err.to_string(),
        }),
        _ => Report::new(err),
    }
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
