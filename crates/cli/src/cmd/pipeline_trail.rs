// SPDX-License-Identifier: Apache-2.0

//! `bookrack pipeline-trail` — render the EXTRACT/STRUCTURE/CHUNK/
//! EMBED audit rows for one book.

use anyhow::{Context, Result};
use bookrack_catalog::Catalog;
use bookrack_config::Config;

use crate::ops_helpers::catalog_only_ops;
use crate::render;

pub fn run(cfg: &Config, book: i64, json: bool) -> Result<()> {
    // Trigger any pending catalog migration up front (the read-only
    // open inside ops does not migrate), then dispatch through ops so
    // the read lands in `mcp_tool_calls` like every other audited read.
    Catalog::open_with_backup(&cfg.catalog_db(), &cfg.backup_dir()).context("open catalog")?;
    let ops = catalog_only_ops(cfg);
    let rows = bookrack_ops::reads::pipeline::show_pipeline_trail(&ops, book)
        .context("read pipeline audit")?;
    if json {
        render::pipeline_trail_json(book, &rows);
    } else {
        render::pipeline_trail(book, &rows);
    }
    Ok(())
}
