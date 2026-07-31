// SPDX-License-Identifier: Apache-2.0

//! `bookrack libraries detect` / `libraries scan` — the read-only,
//! daemon-free surface for asking whether a path is a bookrack data
//! root. Detection itself lives in `bookrack_config::detect`; this
//! module only resolves the CLI's arguments, renders the verdict, and
//! maps it onto an exit code.

use std::path::{Path, PathBuf};

use bookrack_config::{
    AddOptions, AddOutcome, AddReport, ConfigError, DetectError, DetectVerdict, LibraryEntryFields,
    LibraryKind, LibraryManifest, LibraryOpError, ManifestIdentitySeed, ROOT_CONFIG_NAME,
    RootConfigSetError, ScanOutcome, Signal, add_library, detect_library, find_library,
    load_root_config, mounted_volumes, read_root_config_text, registry_target_path, remove_library,
    render_manifest_toml, repoint_library, retired_root_config_key_help, retired_root_config_keys,
    root_config_env_override, scan_for_libraries, set_manifest_index_profile,
    set_root_config_values, upsert_library_entry,
};
use bookrack_session::{RootLock, is_root_lock_conflict};
use eyre::{Report, Result};
use serde::Serialize;

use crate::error::BookrackCliError;
use crate::render::confirm::{ConfirmMode, Confirmation, confirm_destructive};
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
        )?
        .into_io_result()
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

/// Break a uuid clash: offer to move the existing entry to the new
/// path. `--yes` is no answer here — the two resolutions are not more
/// and less cautious versions of one action, they register different
/// things — so the operator is always asked, and a stdin that carries
/// no answer names both commands instead of picking one.
fn resolve_uuid_clash(
    registry_path: &Path,
    path: &Path,
    uuid: &str,
    existing_key: &str,
    existing_path: &Path,
    _yes: bool,
) -> Result<()> {
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
    .map_err(|e| eyre::eyre!("read clash resolution: {e}"))?
    .agreed_or_refuse(
        &format!("libraries add: uuid {uuid} is already registered as '{existing_key}'"),
        &format!(
            "to move it (same library, new path): bookrack libraries add {existing_key} {}; \
             to register a copy (new identity): re-run with --new-uuid",
            path.display()
        ),
    )?;
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
        // Take the data root's exclusive lock before the detect gate
        // and the confirmation: a root in use is refused before the
        // operator types the name, and the lock closes the window in
        // which a daemon could attach while the prompt is open.
        let lock = RootLock::acquire(
            &entry.data_dir,
            std::process::id(),
            "libraries remove --purge",
        )
        .map_err(|err| {
            if is_root_lock_conflict(&err) {
                Report::new(BookrackCliError::LocalUserError {
                    message: format!(
                        "refusing to purge {}: {err}; stop the daemon serving it first",
                        entry.data_dir.display()
                    ),
                })
            } else {
                err
            }
        })?;
        gate_purge_target(&entry.data_dir)?;
        let prompt = format!(
            "This deletes {} for good. Type the library name '{name}' to confirm:",
            entry.data_dir.display()
        );
        if !confirm_destructive(&prompt, ConfirmMode::Hard { token: &name }, yes)
            .map_err(|e| eyre::eyre!("read purge confirmation: {e}"))?
            .agreed_or_refuse(
                &format!("libraries remove '{name}' --purge"),
                "re-run with --yes to purge without a prompt",
            )?
        {
            eprintln!("aborted; nothing removed");
            return Ok(());
        }
        remove_library(&registry_path, &name).map_err(config_error)?;
        // Release before the delete: the lock file sits inside the tree
        // about to go, and Windows refuses to remove a file with an open
        // handle. The registry entry is already gone by now, so nothing
        // routes a new writer here through a library name.
        drop(lock);
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
///
/// `index_profile` is the exception: it is a data contract, not a
/// per-machine preference, so it is written to the library manifest and
/// only cached in the registry entry. Setting or unsetting it here goes
/// down the same path `index-profile apply` uses.
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

    // An `index_profile` reference must resolve and pass static
    // validation before it is written; stamp consistency is not checked
    // here (that is `index-profile apply`'s job).
    for (key, value) in &sets {
        if key == "index_profile"
            && let Some(refusal) = bookrack_runtime::profile::refuse_bad_profile_reference(value)
        {
            return Err(Report::new(BookrackCliError::LocalUserError {
                message: refusal,
            }));
        }
    }

    // `index_profile` is not a config.toml field any more: the manifest
    // holds it. Route it out of the config.toml write and through the
    // same truth-write-then-refresh-caches path `index-profile apply`
    // takes, so both verbs leave one story on disk.
    let profile_set = sets
        .iter()
        .find(|(key, _)| key == "index_profile")
        .map(|(_, value)| value.clone());
    let profile_unset = unset.iter().any(|key| key == "index_profile");
    let file_sets: Vec<(String, String)> = sets
        .iter()
        .filter(|(key, _)| key != "index_profile")
        .cloned()
        .collect();
    // Setting the profile also sweeps a superseded config.toml
    // declaration: it sits above the registry cache in the resolution
    // chain, so leaving it would shadow the cache forever and keep the
    // drift report permanently non-empty.
    let mut file_unset = unset.clone();
    if profile_set.is_some() && !profile_unset && root_config_exists(&data_dir) {
        file_unset.push("index_profile".to_string());
    }

    set_root_config_values(&data_dir, &file_sets, &file_unset).map_err(root_config_set_error)?;

    let mut profile_written = None;
    if profile_set.is_some() || profile_unset {
        let seed = ManifestIdentitySeed {
            name: &name,
            kind: entry.kind,
            description: entry.description.as_deref(),
        };
        set_manifest_index_profile(&data_dir, profile_set.as_deref(), seed).map_err(|e| {
            Report::new(BookrackCliError::LocalUserError {
                message: format!("write index_profile into the library manifest: {e}"),
            })
        })?;
        profile_written = Some(profile_set.clone());
    }

    // Refresh the registry's cached copy after either direction, so the
    // entry does not keep advertising a reference the manifest no longer
    // agrees with. A stale copy is only ever drift now — `doctor` reports
    // it and `libraries scan` repairs it — never a second truth.
    let registry_refreshed = match &profile_written {
        Some(value) if value.as_deref() != entry.index_profile.as_deref() => {
            let fields = LibraryEntryFields {
                data_dir: data_dir.clone(),
                kind: entry.kind,
                description: entry.description.clone(),
                index_profile: value.clone(),
                created_at: entry.created_at.clone(),
                uuid: entry.uuid.clone(),
            };
            upsert_library_entry(&registry_path, &name, &fields).map_err(config_error)?;
            true
        }
        _ => false,
    };

    render_config_write(&name, &data_dir, &sets, &unset, registry_refreshed);
    Ok(())
}

/// Whether the root has a `config.toml` at all.
///
/// Guards the sweep of a superseded `index_profile` declaration: an
/// unset against a root with no file would materialise one. Deliberately
/// a file-existence test rather than a parse — a file carrying a key
/// this binary does not know still needs its stale declaration swept.
pub fn root_config_exists(data_dir: &Path) -> bool {
    data_dir.join(ROOT_CONFIG_NAME).exists()
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
        // The verbatim dump is the one read surface a retired line
        // survives: `load_root_config` refuses the file, so every other
        // command names the key already. Say so here too, rather than
        // handing back a line that resolves to nothing.
        for note in retired_key_notes(&text) {
            eprintln!("{note}");
        }
    }
    Ok(())
}

/// One `note:` line per retired key the raw `config.toml` text declares,
/// in the order the retirement table lists them.
///
/// Matches on a line's leading key name rather than parsing the
/// document: a file carrying a retired key may carry other faults too,
/// and a dump that cannot be parsed still deserves the annotation.
fn retired_key_notes(text: &str) -> Vec<String> {
    let declares = |key: &str| {
        text.lines().any(|line| {
            let line = line.trim_start();
            line.strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
    };
    retired_root_config_keys()
        .filter(|key| declares(key))
        .map(|key| {
            let help = retired_root_config_key_help(key).unwrap_or_default();
            format!("note: `{key}` is retired and resolves to nothing; {help}")
        })
        .collect()
}

/// Report a successful edit: the keys set and unset, plus the advisory
/// notes an operator needs — a set env var shadows the file, and the
/// change only reaches a running daemon on restart. `registry_refreshed`
/// marks an `index_profile` write that also refreshed the registry
/// entry's cached copy.
fn render_config_write(
    name: &str,
    data_dir: &Path,
    sets: &[(String, String)],
    unset: &[String],
    registry_refreshed: bool,
) {
    if ctx().is_json() {
        let value = serde_json::json!({
            "ok": true,
            "name": name,
            "data_dir": data_dir.display().to_string(),
            "set": sets.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
            "unset": unset,
            "registry_refreshed": registry_refreshed,
        });
        println!("{value}");
    } else if !ctx().is_quiet() {
        for (key, value) in sets {
            if key == "index_profile" {
                println!("set {key} = {value:?} (library manifest)");
            } else {
                println!("set {key} = {value:?}");
            }
        }
        for key in unset {
            if key == "index_profile" {
                println!("unset {key} (library manifest)");
            } else {
                println!("unset {key}");
            }
        }
    }

    if ctx().is_quiet() {
        return;
    }
    if sets.iter().any(|(key, _)| key == "index_profile") {
        eprintln!(
            "note: run 'bookrack index-profile current' to compare the profile against the \
             built index"
        );
    }
    if unset.iter().any(|key| key == "index_profile") {
        eprintln!(
            "note: a running daemon keeps serving the old profile (and any supervised \
             reranker) until it restarts"
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
/// data root before its bytes are deleted, so an entry that points at
/// an unrelated directory cannot destroy it. `Confirmed` — a readable
/// manifest — passes on its own. `Probable` rests on two filenames
/// alone, which `touch catalog.db corpus.db` satisfies, so it must
/// also show a real SQLite store behind at least one of the names.
/// The header probe deliberately stops short of a schema-validating
/// open: a root whose schema is older or newer than this binary is
/// still a data root the operator may purge. `Unreadable` refuses
/// with the reason rather than blending into "not a library".
fn gate_purge_target(data_dir: &Path) -> Result<()> {
    let refuse = |message: String| Report::new(BookrackCliError::LocalUserError { message });
    match detect_library(data_dir) {
        Ok(DetectVerdict::Confirmed(_)) => Ok(()),
        Ok(DetectVerdict::Probable { .. }) => {
            if ["catalog.db", "corpus.db"]
                .iter()
                .any(|name| has_sqlite_header(&data_dir.join(name)))
            {
                Ok(())
            } else {
                Err(refuse(format!(
                    "refusing to purge {}: catalog.db / corpus.db are present but neither \
                     is a SQLite database",
                    data_dir.display()
                )))
            }
        }
        Ok(DetectVerdict::Unreadable { reason }) => Err(refuse(format!(
            "refusing to purge {}: the root cannot be assessed: {reason}",
            data_dir.display()
        ))),
        _ => Err(refuse(format!(
            "refusing to purge {}: it is not a confirmed or probable data root",
            data_dir.display()
        ))),
    }
}

/// Whether the file begins with the SQLite format-3 magic bytes. An
/// empty, truncated, or non-SQLite file does not match.
fn has_sqlite_header(path: &Path) -> bool {
    use std::io::Read as _;
    const MAGIC: &[u8; 16] = b"SQLite format 3\0";
    let mut buf = [0u8; 16];
    match std::fs::File::open(path) {
        Ok(mut file) => file.read_exact(&mut buf).is_ok() && &buf == MAGIC,
        Err(_) => false,
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
        // The manifest confirmation crosses `add_library`'s
        // `io::Result<bool>` bound, so an unanswerable prompt arrives
        // here encoded as an I/O error kind. Recover it rather than
        // letting it fall into the generic internal-error path.
        LibraryOpError::Confirm(io)
            if let Some(reason) = Confirmation::no_answer_reason_from_io(io) =>
        {
            Report::new(BookrackCliError::ConfirmationUnanswerable {
                action: "libraries add".to_string(),
                reason,
                hint: "re-run with --yes to write the identity manifest without a prompt"
                    .to_string(),
            })
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_gate_refuses_touched_empty_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("catalog.db"), b"").expect("touch catalog");
        std::fs::write(dir.path().join("corpus.db"), b"").expect("touch corpus");

        let err = gate_purge_target(dir.path()).expect_err("two empty files must not authorise");
        assert!(
            err.to_string().contains("neither is a SQLite database"),
            "unexpected refusal: {err}"
        );
    }

    #[test]
    fn purge_gate_accepts_a_probable_root_with_a_real_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        drop(bookrack_catalog::Catalog::open(&dir.path().join("catalog.db")).expect("seed"));
        std::fs::write(dir.path().join("corpus.db"), b"").expect("touch corpus");

        gate_purge_target(dir.path()).expect("a real store behind the name authorises");
    }

    #[test]
    fn retired_key_notes_annotate_only_the_lines_that_declare_one() {
        let notes = retired_key_notes(
            "ollama_url = \"http://127.0.0.1:11434\"\n  mcp_addr = \"127.0.0.1:9999\"\n",
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("`mcp_addr` is retired"), "{notes:?}");
        // The way out travels with the note.
        assert!(notes[0].contains("BOOKRACK_MCP_ADDR"), "{notes:?}");

        // A file with no retired key gets no note, and neither a
        // commented-out line nor a longer key that merely starts with a
        // retired name counts as a declaration.
        assert!(retired_key_notes("ollama_url = \"http://x:1\"\n").is_empty());
        assert!(retired_key_notes("# mcp_addr = \"127.0.0.1:9999\"\n").is_empty());
        assert!(retired_key_notes("mcp_addr_extra = \"x\"\n").is_empty());

        // Two retired lines in one file are both named, so a single dump
        // tells the operator everything to delete.
        let both = retired_key_notes("mcp_addr = \"127.0.0.1:1\"\nlog_directive = \"debug\"\n");
        assert_eq!(both.len(), 2, "{both:?}");
    }
}
