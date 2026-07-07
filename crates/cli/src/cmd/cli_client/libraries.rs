//! `bookrack libraries {list,info,default,fork}` — control-plane wrapper.

use std::path::PathBuf;

use bookrack_cli::render::ctx;
use bookrack_cli::render::table::{KvTable, RowTable};
use bookrack_runtime::cmd::libraries::CopyMode;
use eyre::Result;
use serde_json::{Value, json};

use crate::LibrariesAction;

use super::helpers;

pub async fn run(action: LibrariesAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    match action {
        LibrariesAction::List { json: _json } => {
            let response = helpers::dispatch(&client, "library.list", Value::Null).await?;
            if ctx().is_json() {
                helpers::print_value(&response);
                return Ok(());
            }
            if ctx().is_quiet() {
                return Ok(());
            }
            render_library_list(&response);
            Ok(())
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

fn render_library_list(response: &Value) {
    let rows = match response.as_array() {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            println!("no libraries registered");
            return;
        }
    };
    let mut table = RowTable::new(["name", "default", "dimension"]);
    for row in rows {
        let name = row.get("name").and_then(Value::as_str).unwrap_or("-");
        let default = row
            .get("default")
            .and_then(Value::as_bool)
            .map(|b| if b { "yes" } else { "" })
            .unwrap_or("");
        let dimension = row
            .get("dimension")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".to_string());
        table.push_row([name.to_string(), default.to_string(), dimension]);
    }
    println!("{}", table.render());
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
