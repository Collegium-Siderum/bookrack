//! `bookrack libraries {list,info,default,fork}` — control-plane wrapper.

use std::path::PathBuf;

use bookrack_runtime::cmd::libraries::CopyMode;
use eyre::Result;
use serde_json::{Value, json};

use crate::LibrariesAction;

use super::helpers;

pub async fn run(action: LibrariesAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    match action {
        LibrariesAction::List { json: _json } => {
            // The control-plane reply is already JSON; the legacy
            // `--json` toggle was about CLI rendering. Both modes
            // emit the same payload now — pretty-printed JSON to
            // stdout.
            helpers::call_and_print(&client, "library.list", Value::Null).await
        }
        LibrariesAction::Info { name } => {
            let params = match name {
                Some(name) => json!({ "name": name }),
                None => Value::Null,
            };
            helpers::call_and_print(&client, "library.info", params).await
        }
        LibrariesAction::Default { name } => {
            helpers::call_and_print(&client, "library.set_default", json!({ "name": name })).await
        }
        LibrariesAction::Fork {
            new_name,
            data_dir,
            copy_mode,
            yes,
        } => {
            if !yes
                && !crate::util::confirm(&format!(
                    "Fork library to '{new_name}' at {}? [yes/no]: ",
                    data_dir.display(),
                ))?
            {
                eprintln!("aborted; no changes written");
                return Ok(());
            }
            let mode = match copy_mode {
                CopyMode::Hardlink => "hardlink",
                CopyMode::Copy => "copy",
            };
            let params = json!({
                "new_name": new_name,
                "data_dir": data_dir,
                "copy_mode": mode,
                "yes": true,
            });
            helpers::call_and_print(&client, "library.fork", params).await
        }
    }
}
