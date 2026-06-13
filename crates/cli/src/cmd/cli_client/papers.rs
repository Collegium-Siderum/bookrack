//! `bookrack papers <action>` — paper-side surface implemented as
//! thin control-plane clients. Ingest submits to the glean queue
//! (`glean.submit`, the paper-side peer of `ingest.submit`); list /
//! find / show / toc route to the read-side `library.*` reads added
//! alongside the book-side surface; `export-csl` calls the new
//! `papers.export_csl` read.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_repl_grammar::{PapersAction, PapersFindArgs, PapersIngestArgs, PapersListArgs};
use serde_json::{Value, json};

use super::helpers;

pub async fn run(action: PapersAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    match action {
        PapersAction::Ingest(args) => ingest(args, runtime_dir).await,
        PapersAction::List(args) => list(args, runtime_dir).await,
        PapersAction::Find(args) => find(args, runtime_dir).await,
        PapersAction::Show { intake_id } => show(intake_id, runtime_dir).await,
        PapersAction::Toc { intake_id } => toc(intake_id, runtime_dir).await,
        PapersAction::ExportCsl { intake_id } => export_csl(intake_id, runtime_dir).await,
        PapersAction::Source { intake_id } => source(intake_id, runtime_dir).await,
    }
}

async fn ingest(args: PapersIngestArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    if args.recursive {
        anyhow::bail!(
            "bookrack papers ingest --recursive is not yet wired through the control plane; \
             pass individual files",
        );
    }
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let mut params = json!({
        "paths": [args.path],
        "force": args.force,
    });
    if let Some(level) = args.priority {
        params["priority"] = Value::String(level);
    }
    helpers::call_with_progress(client, "glean.submit", params).await
}

async fn list(args: PapersListArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let params = json!({
        "limit": args.limit,
        "offset": args.offset,
    });
    helpers::call_and_print(&client, "library.list_papers", params).await
}

async fn find(args: PapersFindArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let params = json!({
        "title_substring": args.title,
        "contributor_name": args.contributor,
        "year": args.year,
        "venue_substring": args.venue,
        "doi": args.doi,
        "limit": args.limit,
        "offset": args.offset,
    });
    helpers::call_and_print(&client, "library.find_papers", params).await
}

async fn show(intake_id: i64, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    helpers::call_and_print(
        &client,
        "library.show_paper",
        json!({ "intake_id": intake_id }),
    )
    .await
}

async fn toc(intake_id: i64, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    helpers::call_and_print(
        &client,
        "library.show_paper_toc",
        json!({ "intake_id": intake_id }),
    )
    .await
}

async fn export_csl(intake_id: i64, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    helpers::call_and_print(
        &client,
        "papers.export_csl",
        json!({ "intake_id": intake_id }),
    )
    .await
}

async fn source(intake_id: i64, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    helpers::call_and_print(
        &client,
        "papers.fetch_source",
        json!({ "intake_id": intake_id }),
    )
    .await
}
