//! `bookrack vectors {rebuild,reembed,reset,drop}` — route each
//! variant through the matching control-plane method.

use std::path::PathBuf;

use bookrack_cli_grammar::WriteVectorsAction;
use eyre::Result;
use serde_json::{Value, json};

use super::helpers;

pub async fn run(action: WriteVectorsAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    match action {
        WriteVectorsAction::Rebuild {
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
            helpers::call_with_progress(client, "vectors.rebuild", params).await
        }
        WriteVectorsAction::Drop => {
            helpers::call_and_print(&client, "vectors.drop", Value::Null).await
        }
        WriteVectorsAction::Reembed {
            book,
            stale_only,
            dry_run,
            yes,
        } => {
            let selectors = json!({
                "book": book,
                "stale_only": stale_only,
            });
            helpers::run_pinned_destructive(
                client,
                "vectors.reembed",
                selectors,
                dry_run,
                yes,
                "About to delete-and-rewrite the chunk rows above.\n\
                 Existing vectors will be overwritten by fresh embeddings\n\
                 from the currently configured model. This is irreversible.\n\
                 Type 'yes' to continue: ",
            )
            .await
        }
        WriteVectorsAction::Reset { yes, resume } => {
            use std::io::IsTerminal;

            use bookrack_cli::render::confirm::{ConfirmMode, confirm_destructive};

            if !yes && !resume {
                if !std::io::stdin().is_terminal() {
                    eyre::bail!("vectors reset drops the existing vectors; pass --yes to confirm");
                }
                eprintln!(
                    "This drops the chunks table and re-embeds every book from the corpus tree."
                );
                eprintln!("The old vectors are unrecoverable.");
                let confirmed = confirm_destructive(
                    "Type RESET (exact, uppercase) to continue:",
                    ConfirmMode::Hard { token: "RESET" },
                    false,
                )
                .map_err(|e| eyre::eyre!("read RESET confirmation: {e}"))?;
                if !confirmed {
                    println!("aborted; no changes written");
                    return Ok(());
                }
            }
            let params = json!({"yes": true, "resume": resume});
            helpers::call_with_progress(client, "vectors.reset", params).await
        }
    }
}
