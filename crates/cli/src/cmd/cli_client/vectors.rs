//! `bookrack vectors {rebuild,reembed,reset,drop}` — route each
//! variant through the matching control-plane method.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_repl_grammar::WriteVectorsAction;
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
            let params = json!({
                "book": book,
                "stale_only": stale_only,
                "dry_run": dry_run,
                "yes": yes,
            });
            helpers::call_with_progress(client, "vectors.reembed", params).await
        }
        WriteVectorsAction::Reset { yes, resume } => {
            let params = json!({"yes": yes, "resume": resume});
            helpers::call_with_progress(client, "vectors.reset", params).await
        }
    }
}
