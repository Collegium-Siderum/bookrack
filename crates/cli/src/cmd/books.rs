// SPDX-License-Identifier: Apache-2.0

//! `bookrack books` — catalog reads (list / find / show / toc /
//! stats). Catalog-only handle: short-lived, no embedder probe.

use anyhow::{Context, Result};
use bookrack_config::Config;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::Ops;
use bookrack_ops::dto::BookFilter;
use bookrack_ops::reads;

use crate::BooksAction;
use crate::ops_helpers::catalog_only_ops;
use crate::render;

pub fn run(cfg: &Config, action: BooksAction) -> Result<()> {
    let ops = catalog_only_ops(cfg);
    match action {
        BooksAction::List {
            limit,
            offset,
            json,
        } => list_all(&ops, limit, offset, json),
        BooksAction::Find {
            title,
            contributor,
            role,
            format,
            limit,
            offset,
            json,
        } => {
            let filter = BookFilter {
                title_substring: title,
                contributor_name: contributor,
                contributor_role: role,
                statuses: Vec::new(),
                format,
            };
            find(&ops, filter, limit, offset, json)
        }
        BooksAction::Show { book, json } => show(&ops, book, json),
        BooksAction::Toc { book, json } => toc(&ops, book, json),
        BooksAction::Stats { json } => stats(&ops, json),
    }
}

fn list_all(ops: &Ops<OllamaEmbedClient>, limit: u32, offset: u32, json: bool) -> Result<()> {
    let result = reads::books::list_books(ops, limit, offset).context("list books via ops")?;
    if json {
        render::books_list_json(&result);
    } else {
        render::books_list(&result);
    }
    Ok(())
}

fn find(
    ops: &Ops<OllamaEmbedClient>,
    filter: BookFilter,
    limit: u32,
    offset: u32,
    json: bool,
) -> Result<()> {
    let result =
        reads::books::find_books(ops, filter, limit, offset).context("find books via ops")?;
    if json {
        render::books_list_json(&result);
    } else {
        render::books_list(&result);
    }
    Ok(())
}

fn show(ops: &Ops<OllamaEmbedClient>, book: i64, json: bool) -> Result<()> {
    let detail = match reads::books::show_book(ops, book) {
        Ok(d) => d,
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => return Err(anyhow::Error::from(e).context("show book via ops")),
    };
    if json {
        render::books_show_json(&detail);
    } else {
        render::books_show(&detail);
    }
    Ok(())
}

fn toc(ops: &Ops<OllamaEmbedClient>, book: i64, json: bool) -> Result<()> {
    let toc = match reads::books::show_toc(ops, book) {
        Ok(t) => t,
        Err(bookrack_ops::OpsError::IntakeNotFound { intake_id }) => {
            anyhow::bail!("no intake registered for book {intake_id}");
        }
        Err(e) => return Err(anyhow::Error::from(e).context("show toc via ops")),
    };
    if toc.nodes.is_empty() {
        if json {
            println!("null");
        } else {
            println!("Book {book}: no TOC nodes.");
        }
        return Ok(());
    }
    if json {
        render::books_toc_json(&toc);
    } else {
        render::books_toc(&toc);
    }
    Ok(())
}

fn stats(ops: &Ops<OllamaEmbedClient>, json: bool) -> Result<()> {
    let stats = reads::books::show_stats(ops).context("show stats via ops")?;
    if json {
        render::books_stats_json(&stats);
    } else {
        render::books_stats(&stats);
    }
    Ok(())
}
