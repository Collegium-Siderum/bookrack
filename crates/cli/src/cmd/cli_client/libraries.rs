//! `bookrack libraries {info,fork}` — control-plane wrapper.

use std::path::PathBuf;

use bookrack_cli::render::ctx;
use bookrack_cli::render::table::KvTable;
use bookrack_runtime::cmd::libraries::CopyMode;
use eyre::Result;
use serde_json::{Value, json};

use crate::LibrariesAction;

use super::helpers;

pub async fn run(action: LibrariesAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    match action {
        LibrariesAction::List { .. } => {
            // `list` renders the on-disk registry offline; `main`
            // dispatches it before reaching this daemon-routed path.
            unreachable!("libraries list is handled offline in main")
        }
        LibrariesAction::Info { name } => {
            let params = match name {
                Some(name) => json!({ "name": name }),
                None => Value::Null,
            };
            let response = helpers::dispatch(&client, "library.info", params).await?;
            if ctx().is_json() {
                helpers::print_value(&response);
                return Ok(());
            }
            if ctx().is_quiet() {
                return Ok(());
            }
            render_library_info(&response);
            Ok(())
        }
        LibrariesAction::Default { .. } => {
            // `libraries default` writes the registry offline; `main`
            // dispatches it before reaching this daemon-routed path.
            unreachable!("libraries default is handled offline in main")
        }
        LibrariesAction::Detect { .. } | LibrariesAction::Scan { .. } => {
            // `detect` / `scan` are read-only and resolve locally; `main`
            // dispatches them before reaching this daemon-routed path.
            unreachable!("libraries detect/scan are handled offline in main")
        }
        LibrariesAction::Add { .. }
        | LibrariesAction::Register { .. }
        | LibrariesAction::Remove { .. }
        | LibrariesAction::Config { .. } => {
            // `add` / `register` / `remove` / `config` edit the registry
            // or a root's `config.toml` offline; `main` dispatches them
            // before reaching this daemon path.
            unreachable!("libraries add/register/remove/config are handled offline in main")
        }
        LibrariesAction::Fork {
            new_name,
            data_dir,
            copy_mode,
            yes,
        } => {
            use bookrack_cli::render::confirm::{ConfirmMode, confirm_destructive};

            let prompt = format!(
                "Fork library to '{new_name}' at {}? Type 'yes' to continue:",
                data_dir.display(),
            );
            let confirmed = confirm_destructive(&prompt, ConfirmMode::Soft, yes)
                .map_err(|e| eyre::eyre!("read fork confirmation: {e}"))?;
            if !confirmed {
                eprintln!("aborted; no changes written");
                return Ok(());
            }
            let mode = match copy_mode {
                CopyMode::Hardlink => "hardlink",
                CopyMode::Copy => "copy",
            };
            let params = json!({
                "new_name": new_name.clone(),
                "data_dir": data_dir.clone(),
                "copy_mode": mode,
                "yes": true,
            });
            let response = helpers::dispatch(&client, "library.fork", params).await?;
            if ctx().is_json() {
                helpers::print_value(&response);
                return Ok(());
            }
            if ctx().is_quiet() {
                return Ok(());
            }
            println!(
                "Forked library to '{new_name}' at {} ({mode}).",
                data_dir.display()
            );
            Ok(())
        }
    }
}

fn render_library_info(response: &Value) {
    let mut table = KvTable::new();
    flatten_into_kv(&mut table, "", response);
    println!("{}", table.render());
}

/// Walk a JSON value into a [`KvTable`]. Scalars become rows with
/// dot-notation keys; arrays render as a compact JSON string so the
/// table stays narrow.
fn flatten_into_kv(table: &mut KvTable, prefix: &str, value: &Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let next = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_into_kv(table, &next, v);
            }
        }
        Value::Null => {
            table.push(prefix, "");
        }
        scalar @ (Value::Bool(_) | Value::Number(_) | Value::String(_)) => {
            let s = match scalar {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            table.push(prefix, s);
        }
        Value::Array(arr) => {
            let compact = serde_json::to_string(arr).unwrap_or_else(|_| "[…]".to_string());
            table.push(prefix, compact);
        }
    }
}
