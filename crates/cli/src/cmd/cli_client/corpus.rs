//! `bookrack corpus rebuild` — control-plane wrapper.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_repl_grammar::CorpusAction;
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
    let params = json!({
        "include_vectors": include_vectors,
        "book": book,
        "stale_only": stale_only,
        "dry_run": dry_run,
        "yes": yes,
    });
    helpers::call_with_progress(client, "corpus.rebuild", params).await
}
