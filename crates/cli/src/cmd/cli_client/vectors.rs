//! `bookrack vectors {rebuild,reembed,reset,drop}` — route each
//! variant through the matching control-plane method.

use std::path::PathBuf;

use bookrack_cli::render::confirm::ConfirmMode;
use bookrack_cli_grammar::WriteVectorsAction;
use eyre::Result;
use serde_json::json;

use super::helpers;
use super::helpers::DestructivePrompt;

pub async fn run(action: WriteVectorsAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
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
        WriteVectorsAction::Drop { yes } => {
            helpers::run_destructive(
                client,
                "vectors.drop",
                json!({}),
                yes,
                false,
                DestructivePrompt {
                    mode: ConfirmMode::Soft,
                    text: "About to drop the ANN index. Search falls back to a full\n\
                           scan until the next `vectors rebuild`. Type 'yes' to continue:",
                    non_tty_hint: "vectors drop removes the ANN index; pass --yes to confirm",
                },
            )
            .await
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
        WriteVectorsAction::Reset { yes, resume } => helpers::run_destructive(
            client,
            "vectors.reset",
            json!({ "resume": resume }),
            yes,
            resume,
            DestructivePrompt {
                mode: ConfirmMode::Hard { token: "RESET" },
                text: "This drops the chunks table and re-embeds every book from the corpus tree.\n\
                           The old vectors are unrecoverable.\n\
                           Type RESET (exact, uppercase) to continue:",
                non_tty_hint: "vectors reset drops the existing vectors; pass --yes to confirm",
            },
        )
        .await,
    }
}
