// SPDX-License-Identifier: Apache-2.0

//! `bookrack index-profile` — list, show, or validate index profiles.
//! The command does not open a data root: built-in profiles are compiled
//! into the binary and user profiles are plain files under the per-user
//! profile directory, so this is a local reflection-and-check surface.

use std::path::PathBuf;

use bookrack_index_profile::{
    Finding, ProfileOrigin, Severity, USER_PROFILE_DIR_NAME, builtin_toml, has_errors,
    list_profiles, resolve, validate,
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
}

pub fn run(action: IndexProfileAction) -> Result<()> {
    match action {
        IndexProfileAction::List { json } => list(json),
        IndexProfileAction::Show { name } => show(&name),
        IndexProfileAction::Validate {
            name,
            allow_unknown_model,
        } => validate_cmd(&name, allow_unknown_model),
    }
}

/// The per-user profile directory, beside `registry.toml`. `None` when no
/// config location can be resolved; the caller then lists built-ins only.
fn user_profile_dir() -> Option<PathBuf> {
    bookrack_config::registry_target_path()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .map(|d| d.join(USER_PROFILE_DIR_NAME))
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
