// SPDX-License-Identifier: Apache-2.0

//! `bookrack index-profile` — list, show, or validate index profiles.
//! The command does not open a data root: built-in profiles are compiled
//! into the binary and user profiles are plain files under the per-user
//! profile directory, so this is a local reflection-and-check surface.

use std::path::PathBuf;

use bookrack_config::{ConfigError, EmbedConfig, list_libraries, load_root_config};
use bookrack_index_profile::{
    Finding, IndexProfile, ProfileOrigin, RerankerKind, Severity, USER_PROFILE_DIR_NAME,
    builtin_toml, has_errors, list_profiles, resolve, validate,
};
use eyre::{Result, bail};

#[derive(clap::Subcommand, Debug)]
pub enum IndexProfileAction {
    /// List every profile — built-ins merged with the user directory —
    /// marking each `[builtin]` or `[user]` and noting any that shadow a
    /// built-in.
    List {
        /// Emit machine-readable JSON instead of the plain listing.
        #[arg(long)]
        json: bool,
    },
    /// Print a profile's source and its static validation result. A user
    /// profile that shadows a built-in prints both.
    Show {
        /// Profile name.
        name: String,
    },
    /// Statically validate a profile and exit non-zero if any finding is
    /// an error.
    Validate {
        /// Profile name.
        name: String,
        /// Skip the "model is known" and "declared dimension matches"
        /// registry checks so an out-of-tree model can still be checked
        /// structurally.
        #[arg(long)]
        allow_unknown_model: bool,
    },
    /// Print the profile a library effectively runs under — its name,
    /// where the reference was declared, the resolved combination — and
    /// compare it against the built index stamps. Offline and read-only.
    Current {
        /// Library to inspect; the registry default when omitted.
        #[arg(long)]
        library: Option<String>,
        /// Emit machine-readable JSON instead of the plain report.
        #[arg(long)]
        json: bool,
    },
    /// Compare two profiles field by field.
    Diff {
        /// First profile name.
        a: String,
        /// Second profile name.
        b: String,
        /// Emit a machine-readable JSON array instead of the table.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(action: IndexProfileAction) -> Result<()> {
    match action {
        IndexProfileAction::List { json } => list(json),
        IndexProfileAction::Show { name } => show(&name),
        IndexProfileAction::Validate {
            name,
            allow_unknown_model,
        } => validate_cmd(&name, allow_unknown_model),
        IndexProfileAction::Current { library, json } => current(library, json),
        IndexProfileAction::Diff { a, b, json } => diff(&a, &b, json),
    }
}

/// The per-user profile directory, beside `registry.toml`. `None` when no
/// config location can be resolved; the caller then lists built-ins only.
fn user_profile_dir() -> Option<PathBuf> {
    crate::profile::user_profile_dir()
}

fn list(json: bool) -> Result<()> {
    // A missing or unresolved directory lists built-ins only.
    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let entries = list_profiles(&dir);

    if json {
        let value = serde_json::Value::Array(
            entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "origin": origin_str(e.origin),
                        "shadows_builtin": e.shadows_builtin,
                    })
                })
                .collect(),
        );
        println!("{value}");
        return Ok(());
    }

    for entry in &entries {
        let tag = format!("[{}]", origin_str(entry.origin));
        if entry.shadows_builtin {
            println!("{:<8} {} (shadows a built-in)", tag, entry.name);
        } else {
            println!("{:<8} {}", tag, entry.name);
        }
    }
    Ok(())
}

fn show(name: &str) -> Result<()> {
    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let user_path = bookrack_index_profile::user_profile_path(&dir, name);
    let user_text = std::fs::read_to_string(&user_path).ok();
    let builtin_text = builtin_toml(name);

    match (&user_text, builtin_text) {
        (Some(user), Some(builtin)) => {
            println!("note: user profile shadows a builtin of the same name");
            println!("# user (effective): {}", user_path.display());
            print!("{user}");
            ensure_trailing_newline(user);
            println!();
            println!("# builtin (shadowed)");
            print!("{builtin}");
            ensure_trailing_newline(builtin);
        }
        (Some(user), None) => {
            println!("# user: {}", user_path.display());
            print!("{user}");
            ensure_trailing_newline(user);
        }
        (None, Some(builtin)) => {
            println!("# builtin: {name}");
            print!("{builtin}");
            ensure_trailing_newline(builtin);
        }
        (None, None) => bail!("unknown index profile '{name}'"),
    }

    // The effective profile (user wins) drives the validation summary.
    let profile =
        resolve(&dir, name)?.ok_or_else(|| eyre::eyre!("unknown index profile '{name}'"))?;
    println!();
    render_findings(&validate(&profile, false));
    Ok(())
}

fn validate_cmd(name: &str, allow_unknown_model: bool) -> Result<()> {
    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let profile =
        resolve(&dir, name)?.ok_or_else(|| eyre::eyre!("unknown index profile '{name}'"))?;
    let findings = validate(&profile, allow_unknown_model);
    render_findings(&findings);
    if has_errors(&findings) {
        let errors = findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count();
        bail!("profile '{name}' has {errors} validation error(s)");
    }
    Ok(())
}

/// Report the profile a library effectively runs under and compare it
/// against the built index stamps. Offline: registry, `config.toml`,
/// profile files, and a read-only corpus open — no daemon involved. The
/// conflicts the daemon refuses to start with surface here as the same
/// errors; a stamp mismatch is a finding in the report, not an error,
/// because reconciling it is `index-profile apply`'s job.
fn current(library: Option<String>, json: bool) -> Result<()> {
    let entries = list_libraries()?.unwrap_or_default();
    let entry = match &library {
        Some(name) => entries
            .iter()
            .find(|e| e.name == *name)
            .ok_or_else(|| eyre::eyre!("no library named '{name}' in the registry"))?,
        None => entries.iter().find(|e| e.is_default).ok_or_else(|| {
            eyre::eyre!("the registry has no default library; pass --library <name>")
        })?,
    };
    let root = load_root_config(&entry.data_dir)?;
    let reference = crate::profile::effective_reference(
        root.index_profile.clone(),
        entry.index_profile.clone(),
    )?;

    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let resolved = match &reference {
        Some((name, origin)) => {
            let profile = resolve(&dir, name)?.ok_or_else(|| {
                eyre::eyre!(
                    "index profile '{name}' is not defined; \
                     `bookrack index-profile list` shows the available names"
                )
            })?;
            Some((profile, *origin))
        }
        None => None,
    };
    if let (Some((profile, _)), Some(explicit)) = (&resolved, root.embed_model.as_deref())
        && explicit != profile.embed.model
    {
        return Err(ConfigError::profile_model_conflict(
            &profile.name,
            &profile.embed.model,
            explicit,
        )
        .into());
    }

    let profile_model = resolved.as_ref().map(|(p, _)| p.embed.model.as_str());
    let effective_model = EmbedConfig::resolve(&root, profile_model).model;
    let built = crate::profile::built_stamps(&entry.data_dir);
    let findings = match (&resolved, &built) {
        (Some((profile, _)), Some(stamps)) => {
            Some(crate::profile::profile_stamp_findings(profile, stamps))
        }
        _ => None,
    };

    if json {
        let value = serde_json::json!({
            "library": entry.name,
            "data_dir": entry.data_dir.display().to_string(),
            "profile": resolved.as_ref().map(|(p, origin)| serde_json::json!({
                "name": p.name,
                "origin": origin.as_str(),
            })),
            "effective_embed_model": effective_model,
            "built_stamps": built.as_ref().map(|(model, dim)| serde_json::json!({
                "embed_model": model,
                "vector_dim": dim,
            })),
            "stamp_findings": findings,
            "consistent": findings.as_ref().map(|f| f.is_empty()),
        });
        println!("{value}");
        return Ok(());
    }

    println!("library: {} ({})", entry.name, entry.data_dir.display());
    match &resolved {
        Some((profile, origin)) => {
            println!("profile: {} (source: {})", profile.name, origin.as_str());
            println!(
                "  embed: {}/{} dim {}",
                profile.embed.backend, profile.embed.model, profile.embed.dim
            );
            println!(
                "  ann: {}{}",
                profile.ann.kind.as_str(),
                ann_params(profile)
            );
            match profile.reranker.kind {
                RerankerKind::None => println!("  reranker: none"),
                RerankerKind::CrossEncoder => println!(
                    "  reranker: cross-encoder ({}) — not implemented; \
                     the daemon refuses to start under this profile",
                    profile.reranker.model.as_deref().unwrap_or("<unset>")
                ),
            }
        }
        None => println!("profile: none (field-level configuration)"),
    }
    println!("effective embed model: {effective_model}");
    match (&built, &findings) {
        (None, _) => println!("stamps: no built index to compare against"),
        (Some((model, dim)), None) => {
            println!("stamps: built index is {model}/{dim} (no profile to compare against)");
        }
        (Some((model, dim)), Some(f)) if f.is_empty() => {
            println!("stamps: consistent with the built index ({model}/{dim})");
        }
        (Some(_), Some(f)) => {
            for finding in f {
                println!("stamp mismatch: {finding}");
            }
            println!("note: `bookrack index-profile apply` (a later release) reconciles this");
        }
    }
    Ok(())
}

/// Compare two profiles field by field.
fn diff(a: &str, b: &str, json: bool) -> Result<()> {
    let dir = user_profile_dir().unwrap_or_else(|| PathBuf::from(USER_PROFILE_DIR_NAME));
    let profile_a = resolve(&dir, a)?.ok_or_else(|| eyre::eyre!("unknown index profile '{a}'"))?;
    let profile_b = resolve(&dir, b)?.ok_or_else(|| eyre::eyre!("unknown index profile '{b}'"))?;
    let rows = diff_rows(&profile_a, &profile_b);

    if json {
        let value = serde_json::Value::Array(
            rows.iter()
                .map(|(field, va, vb)| {
                    serde_json::json!({
                        "field": field,
                        "a": va,
                        "b": vb,
                        "same": va == vb,
                    })
                })
                .collect(),
        );
        println!("{value}");
        return Ok(());
    }

    let field_width = rows.iter().map(|(f, _, _)| f.len()).max().unwrap_or(0);
    let a_width = rows
        .iter()
        .map(|(_, va, _)| va.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(1)
        .max(a.len());
    println!("{:<field_width$}  {:<a_width$}  {}", "field", a, b);
    for (field, va, vb) in &rows {
        let marker = if va == vb { "" } else { "  <- differs" };
        println!(
            "{field:<field_width$}  {:<a_width$}  {}{marker}",
            va.as_deref().unwrap_or("-"),
            vb.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

/// The query-time ANN parameters a summary line shows, skipping the
/// optional ones a profile leaves unset.
fn ann_params(profile: &IndexProfile) -> String {
    let ann = &profile.ann;
    let mut out = format!(" partitions={} nprobes={}", ann.num_partitions, ann.nprobes);
    if let Some(nsv) = ann.num_sub_vectors {
        out.push_str(&format!(" sub_vectors={nsv}"));
    }
    if let Some(bits) = ann.num_bits {
        out.push_str(&format!(" bits={bits}"));
    }
    if let Some(refine) = ann.refine_factor {
        out.push_str(&format!(" refine={refine}"));
    }
    out
}

/// Field-level comparison rows: every dotted field path either profile
/// carries, in profile-a-then-b-extras order, with each side's display
/// value (`None` when that side omits the field).
fn diff_rows(a: &IndexProfile, b: &IndexProfile) -> Vec<(String, Option<String>, Option<String>)> {
    let flat_a = flatten_profile(a);
    let flat_b = flatten_profile(b);
    let mut fields: Vec<String> = flat_a.iter().map(|(f, _)| f.clone()).collect();
    for (field, _) in &flat_b {
        if !fields.contains(field) {
            fields.push(field.clone());
        }
    }
    let value_of = |flat: &[(String, String)], field: &str| {
        flat.iter()
            .find(|(f, _)| f == field)
            .map(|(_, v)| v.clone())
    };
    fields
        .into_iter()
        .map(|field| {
            let va = value_of(&flat_a, &field);
            let vb = value_of(&flat_b, &field);
            (field, va, vb)
        })
        .collect()
}

/// Flatten a profile's serialized form into dotted field paths paired
/// with display values, preserving serialization order.
fn flatten_profile(profile: &IndexProfile) -> Vec<(String, String)> {
    let value = serde_json::to_value(profile).expect("profile serializes");
    let mut out = Vec::new();
    flatten_value("", &value, &mut out);
    out
}

fn flatten_value(prefix: &str, value: &serde_json::Value, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_value(&path, nested, out);
            }
        }
        serde_json::Value::String(s) => out.push((prefix.to_string(), s.clone())),
        other => out.push((prefix.to_string(), other.to_string())),
    }
}

/// Print each finding as `severity: field_path: message`, or a clean-bill
/// line when there are none.
fn render_findings(findings: &[Finding]) {
    if findings.is_empty() {
        println!("ok: no findings");
        return;
    }
    for f in findings {
        println!("{}: {}: {}", f.severity.as_str(), f.field_path, f.message);
    }
}

fn origin_str(origin: ProfileOrigin) -> &'static str {
    match origin {
        ProfileOrigin::BuiltIn => "builtin",
        ProfileOrigin::User => "user",
    }
}

fn ensure_trailing_newline(text: &str) {
    if !text.ends_with('\n') {
        println!();
    }
}
