//! `bookrack metadata {set,clear,ack,approve,reject,advance}` —
//! route the matching write to the control-plane method.

use std::path::PathBuf;

use anyhow::Result;
use bookrack_repl_grammar::WriteMetadataAction;
use serde_json::json;

use super::helpers;

pub async fn run(action: WriteMetadataAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect_or_exit(runtime_dir.as_deref()).await;
    match action {
        WriteMetadataAction::Set {
            book,
            field,
            value,
            reason,
        } => {
            helpers::call_and_print(
                &client,
                "metadata.set",
                json!({"book": book, "field": field, "value": value, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Clear {
            book,
            field,
            reason,
        } => {
            helpers::call_and_print(
                &client,
                "metadata.clear",
                json!({"book": book, "field": field, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Void {
            book,
            field,
            reason,
        } => {
            helpers::call_and_print(
                &client,
                "metadata.void",
                json!({"book": book, "field": field, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Reaudit { book } => {
            helpers::call_and_print(&client, "metadata.reaudit", json!({"book": book})).await
        }
        WriteMetadataAction::ContributorAdd {
            book,
            role,
            name,
            nationality,
            reason,
        } => {
            helpers::call_and_print(
                &client,
                "metadata.contributor_add",
                json!({
                    "book": book,
                    "role": role,
                    "name": name,
                    "nationality": nationality,
                    "reason": reason,
                }),
            )
            .await
        }
        WriteMetadataAction::ContributorRemove {
            book,
            contributor_id,
            reason,
        } => {
            helpers::call_and_print(
                &client,
                "metadata.contributor_remove",
                json!({"book": book, "contributor_id": contributor_id, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Ack { book, reason } => {
            helpers::call_and_print(
                &client,
                "metadata.ack",
                json!({"book": book, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Approve { book, reason } => {
            helpers::call_and_print(
                &client,
                "metadata.approve",
                json!({"book": book, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Reject { book, reason } => {
            helpers::call_and_print(
                &client,
                "metadata.reject",
                json!({"book": book, "reason": reason}),
            )
            .await
        }
        WriteMetadataAction::Advance { book: _ } => {
            anyhow::bail!(
                "metadata advance is not yet wired through the control plane; tracked for a follow-up release",
            )
        }
    }
}
