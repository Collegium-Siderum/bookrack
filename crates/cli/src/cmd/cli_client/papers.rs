//! `bookrack papers <action>` — paper-side surface implemented as
//! thin control-plane clients. Ingest submits to the glean queue
//! (`glean.submit`, the paper-side peer of `ingest.submit`); list /
//! find / show / toc route to the read-side `library.*` reads added
//! alongside the book-side surface; `export-csl` calls the
//! `papers.export_csl` read; `remove` calls the `papers.remove`
//! write.

use std::path::PathBuf;

use bookrack_cli::render::confirm::ConfirmMode;
use bookrack_cli::render::human::truncate_to;
use bookrack_cli::render::table::{KvTable, RowTable};
use bookrack_cli::render::{OutputMode, ctx};
use bookrack_cli_grammar::{
    PapersAction, PapersCorpusAction, PapersDryrunArgs, PapersFindArgs, PapersIngestArgs,
    PapersListArgs, PapersRemoveArgs, PapersStampsAction, PapersVectorsAction,
};
use eyre::Result;
use serde_json::{Value, json};

use super::helpers;
use super::helpers::DestructivePrompt;

pub async fn run(
    action: PapersAction,
    runtime_dir: Option<PathBuf>,
    default_audit_profile: Option<String>,
) -> Result<()> {
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
        PapersAction::Metadata { action } => {
            metadata(action, runtime_dir, default_audit_profile).await
        }
    }
}

async fn metadata(
    action: bookrack_cli_grammar::PapersMetadataAction,
    runtime_dir: Option<PathBuf>,
    default_audit_profile: Option<String>,
) -> Result<()> {
    use bookrack_cli_grammar::PapersMetadataAction;
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    match action {
        PapersMetadataAction::Reaudit {
            intake_id,
            audit_profile,
        } => {
            let mut params = json!({ "intake_id": intake_id });
            if let Some(name) = audit_profile.or(default_audit_profile) {
                params["audit_profile"] = Value::String(name);
            }
            let response =
                helpers::call_with_progress_value(client, "papers.metadata.reaudit", params)
                    .await?;
            let verdict = response
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let confidence = response
                .get("confidence")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let previous_verdict = response
                .get("previous_verdict")
                .and_then(Value::as_str)
                .unwrap_or("unset");
            let previous_confidence = response
                .get("previous_confidence")
                .and_then(Value::as_str)
                .unwrap_or("unset");
            let sentence = format!(
                "Reaudited paper {intake_id}: verdict {verdict} (was {previous_verdict}), \
                 confidence {confidence} (was {previous_confidence})."
            );
            emit_metadata_outcome(&response, sentence);
            Ok(())
        }
        PapersMetadataAction::Set {
            intake_id,
            field,
            value,
            confirmed,
        } => {
            let params = json!({
                "intake_id": intake_id,
                "field": field,
                "value": value,
                "confirmed": confirmed,
            });
            let response =
                helpers::call_with_progress_value(client, "papers.metadata.set", params).await?;
            emit_metadata_outcome(
                &response,
                format!("Set {field} on paper {intake_id} to {value:?}."),
            );
            Ok(())
        }
        PapersMetadataAction::Clear { intake_id, field } => {
            let params = json!({ "intake_id": intake_id, "field": field });
            let response =
                helpers::call_with_progress_value(client, "papers.metadata.clear", params).await?;
            let removed = response
                .get("removed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let sentence = if removed {
                format!("Cleared override on {field} for paper {intake_id}.")
            } else {
                format!("No override on {field} for paper {intake_id} to clear.")
            };
            emit_metadata_outcome(&response, sentence);
            Ok(())
        }
        PapersMetadataAction::Void { intake_id, field } => {
            let params = json!({ "intake_id": intake_id, "field": field });
            let response =
                helpers::call_with_progress_value(client, "papers.metadata.void", params).await?;
            emit_metadata_outcome(&response, format!("Voided {field} on paper {intake_id}."));
            Ok(())
        }
        PapersMetadataAction::Ack { intake_id, notes } => {
            review_status_call(
                client,
                "papers.metadata.ack",
                intake_id,
                notes,
                "acknowledged",
            )
            .await
        }
        PapersMetadataAction::Approve { intake_id, notes } => {
            review_status_call(
                client,
                "papers.metadata.approve",
                intake_id,
                notes,
                "approved",
            )
            .await
        }
        PapersMetadataAction::Reject { intake_id, notes } => {
            review_status_call(
                client,
                "papers.metadata.reject",
                intake_id,
                notes,
                "rejected",
            )
            .await
        }
        PapersMetadataAction::Reopen { intake_id, notes } => {
            review_status_call(
                client,
                "papers.metadata.reopen",
                intake_id,
                notes,
                "pending",
            )
            .await
        }
        PapersMetadataAction::ContributorAdd {
            intake_id,
            role,
            name,
            family,
            given,
            orcid,
        } => {
            let mut params = json!({
                "intake_id": intake_id,
                "role": role,
                "name": name,
            });
            if let Some(family) = family {
                params["family"] = Value::String(family);
            }
            if let Some(given) = given {
                params["given"] = Value::String(given);
            }
            if let Some(orcid) = orcid {
                params["orcid"] = Value::String(orcid);
            }
            let response = helpers::call_with_progress_value(
                client,
                "papers.metadata.contributor_add",
                params,
            )
            .await?;
            let id = response.get("contributor_id").and_then(Value::as_i64);
            let sentence = format!(
                "Added contributor to paper {intake_id} (contributor_id={}).",
                id.map_or("unknown".to_string(), |i| i.to_string()),
            );
            emit_metadata_outcome(&response, sentence);
            Ok(())
        }
        PapersMetadataAction::ContributorRemove { contributor_id } => {
            let params = json!({ "contributor_id": contributor_id });
            let response = helpers::call_with_progress_value(
                client,
                "papers.metadata.contributor_remove",
                params,
            )
            .await?;
            let removed = response
                .get("removed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let sentence = if removed {
                format!("Removed contributor {contributor_id}.")
            } else {
                format!("No contributor {contributor_id} to remove.")
            };
            emit_metadata_outcome(&response, sentence);
            Ok(())
        }
    }
}

async fn review_status_call(
    client: std::sync::Arc<bookrack_control_client::ControlClient>,
    method: &str,
    intake_id: i64,
    notes: Option<String>,
    pretty_status: &str,
) -> Result<()> {
    let mut params = json!({ "intake_id": intake_id });
    if let Some(notes) = notes {
        params["notes"] = Value::String(notes);
    }
    let response = helpers::call_with_progress_value(client, method, params).await?;
    emit_metadata_outcome(
        &response,
        format!("Paper {intake_id} review status is now {pretty_status}."),
    );
    Ok(())
}

/// Prints the outcome of one `papers metadata` write in the mode the
/// operator asked for.
fn emit_metadata_outcome(response: &Value, sentence: String) {
    if let Some(text) = metadata_outcome_line(ctx().output(), response, sentence) {
        println!("{text}");
    }
}

/// Output-mode gate shared by every `papers metadata` write: `Quiet`
/// prints nothing and lets the exit code answer, `Json` prints the
/// server's response payload so a script parses one object, `Human`
/// prints the one-line summary.
fn metadata_outcome_line(mode: OutputMode, response: &Value, sentence: String) -> Option<String> {
    match mode {
        OutputMode::Quiet => None,
        OutputMode::Json => {
            Some(serde_json::to_string_pretty(response).unwrap_or_else(|_| response.to_string()))
        }
        OutputMode::Human => Some(sentence),
    }
}

async fn dryrun(args: PapersDryrunArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    let params = json!({
        "path": args.path,
        "out": args.out,
        "no_chunk": args.no_chunk,
    });
    let value = helpers::call_with_progress_value(client, "papers.dryrun", params).await?;
    let outcome: bookrack_runtime::cmd::papers_dryrun::PapersDryrunRunOutcome =
        serde_json::from_value(value)
            .map_err(|e| eyre::eyre!("papers.dryrun response did not match: {e}"))?;
    bookrack_runtime::cmd::papers_dryrun::render_outcome(&outcome, args.stdout)
}

async fn corpus(action: PapersCorpusAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    match action {
        PapersCorpusAction::Rebuild {
            include_vectors,
            paper,
            stale_only,
            dry_run,
            yes,
        } => {
            let selectors = json!({
                "include_vectors": include_vectors,
                "paper": paper,
                "stale_only": stale_only,
            });
            let prompt = if include_vectors {
                "About to overwrite papers_corpus.db node rows for the intakes\n\
                 above, then re-embed each paper's abstract into lancedb_papers.\n\
                 This is irreversible (the existing corpus tree is replaced).\n\
                 Type 'yes' to continue: "
            } else {
                "About to overwrite papers_corpus.db node rows for the intakes\n\
                 above. lancedb_papers will retain its current chunks; the\n\
                 index_meta build stamps are re-stamped from the existing chunks\n\
                 so search can continue to run. Re-embed with\n\
                 `bookrack papers vectors reembed` if you bumped the chunking\n\
                 or normalization algorithm. This is irreversible (the existing\n\
                 corpus tree is replaced).\n\
                 Type 'yes' to continue: "
            };
            helpers::run_pinned_destructive(
                client,
                "papers.corpus_rebuild",
                selectors,
                dry_run,
                yes,
                prompt,
            )
            .await
        }
    }
}

async fn vectors(action: PapersVectorsAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
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
        PapersVectorsAction::Drop { yes } => {
            helpers::run_destructive(
                client,
                "papers.vectors_drop",
                json!({}),
                yes,
                false,
                DestructivePrompt {
                    mode: ConfirmMode::Soft,
                    text: "About to drop the ANN index over the paper vector store.\n\
                           Search falls back to a full scan until the next\n\
                           `papers vectors rebuild`. Type 'yes' to continue:",
                    unanswered_hint:
                        "papers vectors drop removes the paper ANN index; pass --yes to confirm",
                },
            )
            .await
        }
        PapersVectorsAction::Reembed {
            paper,
            stale_only,
            dry_run,
            yes,
        } => {
            let selectors = json!({
                "paper": paper,
                "stale_only": stale_only,
            });
            helpers::run_pinned_destructive(
                client,
                "papers.vectors_reembed",
                selectors,
                dry_run,
                yes,
                "About to delete-and-rewrite the paper chunk rows above.\n\
                 Existing vectors will be overwritten by fresh embeddings\n\
                 from the currently configured model. This is irreversible.\n\
                 Type 'yes' to continue: ",
            )
            .await
        }
        PapersVectorsAction::Reset { yes, resume } => {
            helpers::run_destructive(
                client,
                "papers.vectors_reset",
                json!({ "resume": resume }),
                yes,
                resume,
                DestructivePrompt {
                    mode: ConfirmMode::Hard { token: "RESET" },
                    text: "This drops the paper chunks table and re-embeds every paper abstract from the corpus tree.\n\
                           The old vectors are unrecoverable.\n\
                           Type RESET (exact, uppercase) to continue:",
                    unanswered_hint:
                        "papers vectors reset drops the existing paper vectors; pass --yes to confirm",
                },
            )
            .await
        }
    }
}

async fn stamps(action: PapersStampsAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    match action {
        PapersStampsAction::Reconcile => {
            helpers::call_and_print(&client, "papers.stamps_reconcile", json!({})).await
        }
    }
}

async fn remove(args: PapersRemoveArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    let selectors = json!({
        "intake_id": args.intake_id,
        "sha": args.sha,
    });
    helpers::run_pinned_destructive(
        client,
        "papers.remove",
        selectors,
        args.dry_run,
        args.yes,
        "About to delete this paper from every store. This is\n\
         irreversible (vector tombstones are not recoverable).\n\
         Audit rows are preserved. Type 'yes' to continue: ",
    )
    .await
}

async fn ingest(args: PapersIngestArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let label_owned = args
        .path
        .file_name()
        .and_then(|f| f.to_str())
        .map(String::from)
        .unwrap_or_else(|| args.path.display().to_string());

    let paths = if args.path.is_dir() {
        if !args.recursive {
            eyre::bail!(
                "{} is a directory; pass --recursive to enqueue every .pdf under it",
                args.path.display(),
            );
        }
        let mut collected = crate::util::collect_pdf_files(&args.path);
        if collected.is_empty() {
            eyre::bail!(
                "no supported paper files found under {}",
                args.path.display()
            );
        }
        collected.sort();
        collected
    } else {
        vec![args.path]
    };
    let client = helpers::connect(runtime_dir.as_deref()).await?;

    // Subscribe before issuing the RPC so `queue.tick` events fired
    // by the worker between submit-ack and the wait loop's first
    // `recv` cannot slip past us.
    let rx = client
        .subscribe()
        .await
        .map_err(eyre::Report::from)
        .map_err(|e| e.wrap_err("subscribe to control-plane events"))?;

    let mut params = json!({
        "paths": paths,
        "force": args.force,
    });
    if let Some(level) = args.priority {
        params["priority"] = Value::String(level);
    }
    let response = helpers::dispatch(&client, "glean.submit", params).await?;
    let job_ids = helpers::extract_job_ids(&response);

    if args.no_wait || job_ids.is_empty() {
        helpers::print_value(&response);
        return Ok(());
    }

    let report = helpers::await_jobs(rx, &job_ids).await?;
    helpers::finalize_job_batch(&report, "Ingested", &label_owned)
}

async fn list(args: PapersListArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    let params = json!({
        "limit": args.limit,
        "offset": args.offset,
    });
    let response = helpers::dispatch(&client, "library.list_papers", params).await?;
    emit_paper_list(&response);
    Ok(())
}

async fn find(args: PapersFindArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    let params = json!({
        "title_substring": args.title,
        "contributor_name": args.contributor,
        "year": args.year,
        "venue_substring": args.venue,
        "doi": args.doi,
        "limit": args.limit,
        "offset": args.offset,
    });
    let response = helpers::dispatch(&client, "library.find_papers", params).await?;
    emit_paper_list(&response);
    Ok(())
}

async fn show(intake_id: i64, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    let response = helpers::dispatch(
        &client,
        "library.show_paper",
        json!({ "intake_id": intake_id }),
    )
    .await?;
    if ctx().is_json() {
        helpers::print_value(&response);
        return Ok(());
    }
    if ctx().is_quiet() {
        return Ok(());
    }
    render_paper_detail(&response);
    Ok(())
}

fn emit_paper_list(response: &Value) {
    if ctx().is_json() {
        helpers::print_value(response);
        return;
    }
    if ctx().is_quiet() {
        return;
    }
    render_paper_list(response);
}

fn render_paper_list(response: &Value) {
    let papers = response.get("papers").and_then(Value::as_array);
    let rows = match papers {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            println!("no papers match");
            return;
        }
    };
    let mut table = RowTable::new(["id", "title", "author", "year", "container"]);
    for row in rows {
        let id = row
            .get("intake_id")
            .and_then(Value::as_i64)
            .map(|i| i.to_string())
            .unwrap_or_else(|| "-".to_string());
        let title = row
            .get("title")
            .and_then(Value::as_str)
            .map(|s| truncate_to(s, 48))
            .unwrap_or_else(|| "-".to_string());
        let author = row
            .get("top_contributor")
            .and_then(Value::as_str)
            .map(|s| truncate_to(s, 24))
            .unwrap_or_else(|| "-".to_string());
        let year = row
            .get("year")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string();
        let container = row
            .get("container_title")
            .and_then(Value::as_str)
            .map(|s| truncate_to(s, 24))
            .unwrap_or_else(|| "-".to_string());
        table.push_row([id, title, author, year, container]);
    }
    println!("{}", table.render());
    let total = response.get("total").and_then(Value::as_u64).unwrap_or(0);
    let truncated = response
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if truncated {
        println!(
            "(showing {} of {total}; pass --limit to see more)",
            rows.len()
        );
    } else if total as usize != rows.len() {
        println!("({} of {total})", rows.len());
    }
}

fn render_paper_detail(response: &Value) {
    println!("{}", format_paper_detail(response));
}

/// Keys the detail card renders as their own row, from the top-level
/// response fields that carry the same values. The `effective_biblio`
/// section skips them: the title would repeat verbatim, and the
/// abstract body would land in the card unbounded next to the
/// one-line `abstract` row. Both stay in the `--json` payload.
const BIBLIO_KEYS_ROWED_SEPARATELY: [&str; 2] = ["abstract_text", "title"];

/// Renders one `library.show_paper` response as a key-value card:
/// the intake identity, the audit verdict and the profile that
/// produced it, the effective biblio section, contributor and
/// override counts, and the first line of the abstract.
fn format_paper_detail(response: &Value) -> String {
    let mut t = KvTable::new();
    if let Some(id) = response.get("intake_id").and_then(Value::as_i64) {
        t.push("intake_id", id.to_string());
    }
    for key in ["title", "status", "format"] {
        if let Some(val) = response.get(key).and_then(Value::as_str) {
            t.push(key, val);
        }
    }
    if let Some(audit) = response.get("audit").and_then(Value::as_object) {
        let verdict = audit.get("verdict").and_then(Value::as_str).unwrap_or("-");
        let confidence = audit
            .get("confidence")
            .and_then(Value::as_str)
            .unwrap_or("-");
        t.push("audit", format!("{verdict} ({confidence})"));
        let name = audit
            .get("profile_name")
            .and_then(Value::as_str)
            .unwrap_or("-");
        let profile = match audit.get("profile_fingerprint").and_then(Value::as_str) {
            Some(fp) => format!("{name} @ {fp}"),
            None => name.to_string(),
        };
        t.push("profile", profile);
    }
    if let Some(biblio) = response.get("effective_biblio").and_then(Value::as_object) {
        for (k, v) in biblio {
            if BIBLIO_KEYS_ROWED_SEPARATELY.contains(&k.as_str()) {
                continue;
            }
            let s = v
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| v.to_string());
            t.push(format!("biblio.{k}"), s);
        }
    }
    if let Some(arr) = response.get("contributors").and_then(Value::as_array) {
        t.push("contributors", arr.len().to_string());
    }
    if let Some(arr) = response.get("overrides").and_then(Value::as_array) {
        t.push("overrides", arr.len().to_string());
    }
    if let Some(abs) = response.get("abstract_text").and_then(Value::as_str) {
        let first_line = abs.lines().next().unwrap_or("");
        t.push("abstract", truncate_to(first_line, 80));
    }
    t.render()
}

async fn toc(intake_id: i64, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    helpers::call_and_print(
        &client,
        "library.show_paper_toc",
        json!({ "intake_id": intake_id }),
    )
    .await
}

async fn export_csl(intake_id: i64, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    helpers::call_and_print(
        &client,
        "papers.export_csl",
        json!({ "intake_id": intake_id }),
    )
    .await
}

async fn source(intake_id: i64, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    helpers::call_and_print(
        &client,
        "papers.fetch_source",
        json!({ "intake_id": intake_id }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABSTRACT: &str = "We revisit the sampler and show that the bound is tight for every \
                            admissible input, then extend the argument to the streaming case.";

    fn detail_response() -> Value {
        json!({
            "intake_id": 7,
            "title": "On Tight Bounds",
            "status": "Ready",
            "format": "pdf",
            "effective_biblio": {
                "title": "On Tight Bounds",
                "year": "2020",
                "container_title": "Proceedings of Nothing",
                "abstract_text": ABSTRACT,
            },
            "contributors": [{ "name": "Rivera" }],
            "overrides": [],
            "abstract_text": ABSTRACT,
        })
    }

    const SENTENCE: &str = "Cleared override on title for paper 7.";

    fn write_response() -> Value {
        json!({ "removed": true, "field": "title", "intake_id": 7 })
    }

    #[test]
    fn a_metadata_write_prints_nothing_in_quiet_mode() {
        assert_eq!(
            metadata_outcome_line(OutputMode::Quiet, &write_response(), SENTENCE.to_string()),
            None
        );
    }

    #[test]
    fn a_metadata_write_prints_the_server_payload_in_json_mode() {
        let line = metadata_outcome_line(OutputMode::Json, &write_response(), SENTENCE.to_string())
            .expect("json mode prints the payload");
        assert!(
            !line.contains(SENTENCE),
            "the human sentence reached a --json stream:\n{line}"
        );
        let parsed: Value = serde_json::from_str(&line).expect("json mode prints one JSON object");
        assert_eq!(parsed, write_response());
    }

    #[test]
    fn a_metadata_write_prints_the_summary_in_human_mode() {
        assert_eq!(
            metadata_outcome_line(OutputMode::Human, &write_response(), SENTENCE.to_string()),
            Some(SENTENCE.to_string())
        );
    }

    #[test]
    fn detail_card_carries_the_abstract_once_and_truncated() {
        let card = format_paper_detail(&detail_response());
        assert!(
            !card.contains(ABSTRACT),
            "the abstract body reached the card verbatim:\n{card}"
        );
        assert!(
            !card.contains("biblio.abstract_text"),
            "the abstract has its own row already:\n{card}"
        );
        assert!(
            card.contains(
                "We revisit the sampler and show that the bound is tight for every admissible in…"
            ),
            "missing the truncated abstract row:\n{card}"
        );
    }

    #[test]
    fn detail_card_names_the_title_once() {
        let card = format_paper_detail(&detail_response());
        assert!(
            !card.contains("biblio.title"),
            "the title has its own row already:\n{card}"
        );
        assert_eq!(
            card.matches("On Tight Bounds").count(),
            1,
            "the title is rendered more than once:\n{card}"
        );
    }

    #[test]
    fn detail_card_keeps_the_remaining_biblio_fields() {
        let card = format_paper_detail(&detail_response());
        for needle in [
            "biblio.year",
            "2020",
            "biblio.container_title",
            "Proceedings of Nothing",
        ] {
            assert!(card.contains(needle), "missing {needle:?} in:\n{card}");
        }
    }
}
