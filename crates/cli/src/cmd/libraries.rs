// SPDX-License-Identifier: Apache-2.0

//! `bookrack libraries list` — render the registered library entries
//! straight from the on-disk registry.

use anyhow::{Context, Result};

use crate::render;

pub fn list(json: bool) -> Result<()> {
    let entries = bookrack_config::list_libraries().context("list libraries")?;
    if json {
        render::libraries_list_json(entries.as_deref());
    } else {
        render::libraries_list(entries.as_deref());
    }
    Ok(())
}
