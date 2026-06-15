//! `bookrack papers <action>` — paper-side surface implemented as
//! thin control-plane clients. Ingest submits to the glean queue
//! (`glean.submit`, the paper-side peer of `ingest.submit`); list /
//! find / show / toc route to the read-side `library.*` reads added
//! alongside the book-side surface; `export-csl` calls the
//! `papers.export_csl` read; `remove` calls the `papers.remove`
//! write.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_repl_grammar::{
    PapersAction, PapersCorpusAction, PapersDryrunArgs, PapersFindArgs, PapersIngestArgs,
    PapersListArgs, PapersRemoveArgs, PapersStampsAction, PapersVectorsAction,
};
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
        PapersAction::Remove(args) => remove(args, runtime_dir).await,
        PapersAction::Corpus { action } => corpus(action, runtime_dir).await,
        PapersAction::Vectors { action } => vectors(action, runtime_dir).await,
        PapersAction::Stamps { action } => stamps(action, runtime_dir).await,
        PapersAction::Dryrun(args) => dryrun(args, runtime_dir).await,
        PapersAction::Metadata { action } => metadata(action, runtime_dir).await,
    }
}

async fn metadata(
    action: bookrack_repl_grammar::PapersMetadataAction,
    runtime_dir: Option<PathBuf>,
) -> Result<()> {
    use bookrack_repl_grammar::PapersMetadataAction;
    match action {
        PapersMetadataAction::Reaudit {
            intake_id,
            audit_profile,
        } => {
            let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
            let mut params = json!({ "intake_id": intake_id });
            if let Some(name) = audit_profile {
                params["audit_profile"] = Value::String(name);
            }
            let value =
                helpers::call_with_progress_value(client, "papers.metadata.reaudit", params)
                    .await?;
            let verdict = value
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let confidence = value
                .get("confidence")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let previous_verdict = value
                .get("previous_verdict")
                .and_then(Value::as_str)
                .unwrap_or("unset");
            let previous_confidence = value
                .get("previous_confidence")
                .and_then(Value::as_str)
                .unwrap_or("unset");
            println!(
                "Reaudited paper {intake_id}: verdict {verdict} (was {previous_verdict}), \
                 confidence {confidence} (was {previous_confidence})."
            );
            Ok(())
        }
    }
}

async fn dryrun(args: PapersDryrunArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let params = json!({
        "path": args.path,
        "out": args.out,
        "no_chunk": args.no_chunk,
    });
    let value = helpers::call_with_progress_value(client, "papers.dryrun", params).await?;
    let outcome: bookrack_runtime::cmd::papers_dryrun::PapersDryrunRunOutcome =
        serde_json::from_value(value)
            .map_err(|e| anyhow::anyhow!("papers.dryrun response did not match: {e}"))?;
    bookrack_runtime::cmd::papers_dryrun::render_outcome(&outcome, args.stdout)
}

async fn corpus(action: PapersCorpusAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    match action {
        PapersCorpusAction::Rebuild {
            include_vectors,
            paper,
            stale_only,
            dry_run,
            yes,
        } => {
            let params = json!({
                "include_vectors": include_vectors,
                "paper": paper,
                "stale_only": stale_only,
                "dry_run": dry_run,
                "yes": yes,
            });
            helpers::call_and_print(&client, "papers.corpus_rebuild", params).await
        }
    }
}

async fn vectors(action: PapersVectorsAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    match action {
        PapersVectorsAction::Rebuild {
            kind,
            num_partitions,
            num_sub_vectors,
            num_bits,
            nprobes,
            refine_factor,
        } => {
            let params = json!({
                "kind": kind,
                "num_partitions": num_partitions,
                "num_sub_vectors": num_sub_vectors,
                "num_bits": num_bits,
                "nprobes": nprobes,
                "refine_factor": refine_factor,
            });
            helpers::call_and_print(&client, "papers.vectors_rebuild", params).await
        }
        PapersVectorsAction::Drop => {
            helpers::call_and_print(&client, "papers.vectors_drop", json!({})).await
        }
        PapersVectorsAction::Reembed {
            paper,
            stale_only,
            dry_run,
            yes,
        } => {
            let params = json!({
                "paper": paper,
                "stale_only": stale_only,
                "dry_run": dry_run,
                "yes": yes,
            });
            helpers::call_and_print(&client, "papers.vectors_reembed", params).await
        }
        PapersVectorsAction::Reset { yes, resume } => {
            let params = json!({ "yes": yes, "resume": resume });
            helpers::call_and_print(&client, "papers.vectors_reset", params).await
        }
    }
}

async fn stamps(action: PapersStampsAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    match action {
        PapersStampsAction::Reconcile => {
            helpers::call_and_print(&client, "papers.stamps_reconcile", json!({})).await
        }
    }
}

async fn remove(args: PapersRemoveArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let params = json!({
        "intake_id": args.intake_id,
        "sha": args.sha,
        "dry_run": args.dry_run,
        "yes": args.yes,
    });
    helpers::call_and_print(&client, "papers.remove", params).await
}

async fn ingest(args: PapersIngestArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let paths = if args.path.is_dir() {
        if !args.recursive {
            anyhow::bail!(
                "{} is a directory; pass --recursive to enqueue every .pdf under it",
                args.path.display(),
            );
        }
        let mut collected = crate::util::collect_pdf_files(&args.path);
        if collected.is_empty() {
            anyhow::bail!(
                "no supported paper files found under {}",
                args.path.display()
            );
        }
        collected.sort();
        collected
    } else {
        vec![args.path]
    };
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let mut params = json!({
        "paths": paths,
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
