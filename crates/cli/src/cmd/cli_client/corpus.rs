//! `bookrack corpus rebuild` — control-plane wrapper.
//!
//! Drives the daemon's two-step pinned destructive RPC: send
//! `corpus.rebuild` with `dry_run = true`, print the plan, prompt
//! (unless `--yes`), then send the execute leg under the returned
//! `plan_id` so the daemon acts on exactly the target set the
//! operator confirmed.

use std::path::PathBuf;

use bookrack_cli_grammar::CorpusAction;
use eyre::Result;
use serde_json::json;

use super::helpers;

pub async fn run(action: CorpusAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    let CorpusAction::Rebuild {
        include_vectors,
        book,
        stale_only,
        dry_run,
        yes,
    } = action;
    let selectors = json!({
        "include_vectors": include_vectors,
        "book": book,
        "stale_only": stale_only,
    });
    let prompt = if include_vectors {
        "About to overwrite corpus.db node rows for the intakes above,\n\
         then re-embed each book's chunks into LanceDB. This is\n\
         irreversible (the existing corpus tree is replaced).\n\
         Type 'yes' to continue: "
    } else {
        "About to overwrite corpus.db node rows for the intakes above.\n\
         LanceDB will retain its current chunks; the index_meta build\n\
         stamps are re-stamped from the existing chunks so search can\n\
         continue to run. Re-embed with `bookrack vectors reembed`\n\
         if you bumped the chunking or normalization algorithm.\n\
         This is irreversible (the existing corpus tree is replaced).\n\
         Type 'yes' to continue: "
    };
    helpers::run_pinned_destructive(client, "corpus.rebuild", selectors, dry_run, yes, prompt).await
}
