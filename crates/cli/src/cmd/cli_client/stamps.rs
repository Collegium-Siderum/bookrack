//! `bookrack stamps reconcile` — control-plane wrapper.

use std::path::PathBuf;

use bookrack_cli_grammar::StampsAction;
use eyre::Result;
use serde_json::Value;

use super::helpers;
use super::vectors::library_of;

pub async fn run(action: StampsAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    let StampsAction::Reconcile = action;
    let reply = helpers::call_with_progress_value(client, "stamps.reconcile", Value::Null).await?;
    helpers::emit_outcome(&reply, reconcile_sentence(&reply));
    Ok(())
}

/// `stamps reconcile` and its paper peer: the four stamps written.
pub(super) fn reconcile_sentence(reply: &Value) -> Option<String> {
    let library = library_of(reply)?;
    let model = reply.get("embed_model")?.as_str()?;
    let dim = reply.get("vector_dim")?.as_u64()?;
    let chunk = reply.get("chunk_version")?.as_u64()?;
    let normalize = reply.get("normalize_version")?.as_u64()?;
    Some(format!(
        "Reconciled index stamps on {library}: embed_model={model} vector_dim={dim} \
         chunk_version={chunk} normalize_version={normalize}."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply() -> Value {
        json!({
            "ok": true, "library": "alpha", "embed_model": "m",
            "vector_dim": 8, "chunk_version": 3, "normalize_version": 2,
        })
    }

    #[test]
    fn a_reconcile_reply_reads_as_the_stamps_written() {
        assert_eq!(
            reconcile_sentence(&reply()).as_deref(),
            Some(
                "Reconciled index stamps on alpha: embed_model=m vector_dim=8 \
                 chunk_version=3 normalize_version=2."
            )
        );
    }

    #[test]
    fn a_reconcile_reply_without_a_stamp_has_no_sentence() {
        for field in [
            "library",
            "embed_model",
            "vector_dim",
            "chunk_version",
            "normalize_version",
        ] {
            let mut reply = reply();
            reply.as_object_mut().unwrap().remove(field);
            assert_eq!(reconcile_sentence(&reply), None, "without {field}");
        }
    }
}
