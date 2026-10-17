//! `bookrack vectors {rebuild,reembed,reset,drop}` — route each
//! variant through the matching control-plane method.

use std::path::PathBuf;

use bookrack_cli::render::confirm::ConfirmMode;
use bookrack_cli_grammar::WriteVectorsAction;
use eyre::Result;
use serde_json::{Value, json};

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
            let reply =
                helpers::call_with_progress_value(client, "vectors.rebuild", params).await?;
            helpers::emit_outcome(&reply, rebuild_sentence(&reply));
            Ok(())
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
                    unanswered_hint: "vectors drop removes the ANN index; pass --yes to confirm",
                    outcome: drop_sentence,
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
                unanswered_hint: "vectors reset drops the existing vectors; pass --yes to confirm",
                outcome: |reply| reset_sentence(reply, "intake"),
            },
        )
        .await,
    }
}

/// The library a write reply names.
pub(super) fn library_of(reply: &Value) -> Option<&str> {
    reply.get("library")?.as_str()
}

/// `vectors rebuild` and its paper peer: the index built and its
/// partition count.
pub(super) fn rebuild_sentence(reply: &Value) -> Option<String> {
    let library = library_of(reply)?;
    let kind = reply.get("kind")?.as_str()?;
    let partitions = reply.get("num_partitions")?.as_u64()?;
    Some(format!(
        "Rebuilt ANN index on {library}: kind={kind} np={partitions}."
    ))
}

/// `vectors drop` and its paper peer: the kind search now falls back
/// to.
pub(super) fn drop_sentence(reply: &Value) -> Option<String> {
    let library = library_of(reply)?;
    let kind = reply.get("kind")?.as_str()?;
    Some(format!(
        "Dropped ANN index on {library}; search falls back to {kind}."
    ))
}

/// `vectors reset` and its paper peer, counting `noun`s: what the pass
/// re-embedded, what it skipped, and the restart the new model needs.
/// A resume that found nothing waiting says so instead.
pub(super) fn reset_sentence(reply: &Value, noun: &str) -> Option<String> {
    let library = library_of(reply)?;
    let resumed = reply.get("resumed")?.as_bool()?;
    let reembedded = reply.get("intakes_reembedded")?.as_u64()?;
    let written = reply.get("chunks_written")?.as_u64()?;
    let skipped: Vec<String> = reply
        .get("skipped_empty")?
        .as_array()?
        .iter()
        .map(|id| id.as_i64().map(|id| id.to_string()))
        .collect::<Option<_>>()?;
    if resumed && reembedded == 0 && skipped.is_empty() {
        return Some(format!(
            "Nothing to resume on {library}: no {noun} is waiting to be re-embedded. \
             To start a fresh reset, drop --resume."
        ));
    }
    let mut lines = vec![format!(
        "Reset {library}: {reembedded} {noun}(s) re-embedded, {written} chunk row(s) written."
    )];
    if !skipped.is_empty() {
        lines.push(format!(
            "Skipped, with nothing to chunk: {}.",
            skipped.join(", ")
        ));
    }
    lines.push("Restart the daemon so the new model takes effect.".to_string());
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebuild_reply_reads_as_the_index_built() {
        let reply = json!({
            "ok": true, "library": "alpha", "kind": "ivf-flat",
            "num_partitions": 64, "nprobes": 40,
        });
        assert_eq!(
            rebuild_sentence(&reply).as_deref(),
            Some("Rebuilt ANN index on alpha: kind=ivf-flat np=64.")
        );
    }

    /// A reply missing any field the sentence states is shown as it
    /// came: a daemon that answers a bare `ok` is not worded as a
    /// rebuild of some kind on some library.
    #[test]
    fn a_rebuild_reply_without_a_stated_field_has_no_sentence() {
        let full =
            json!({ "ok": true, "library": "alpha", "kind": "ivf-flat", "num_partitions": 64 });
        for field in ["library", "kind", "num_partitions"] {
            let mut reply = full.clone();
            reply.as_object_mut().unwrap().remove(field);
            assert_eq!(rebuild_sentence(&reply), None, "without {field}");
        }
        assert_eq!(rebuild_sentence(&json!({ "ok": true })), None);
    }

    #[test]
    fn a_drop_reply_reads_as_the_fallback() {
        let reply = json!({ "ok": true, "library": "alpha", "kind": "brute-force" });
        assert_eq!(
            drop_sentence(&reply).as_deref(),
            Some("Dropped ANN index on alpha; search falls back to brute-force.")
        );
        assert_eq!(
            drop_sentence(&json!({ "ok": true, "library": "alpha" })),
            None
        );
        assert_eq!(
            drop_sentence(&json!({ "ok": true, "kind": "brute-force" })),
            None
        );
    }

    fn reset_reply() -> Value {
        json!({
            "ok": true, "library": "alpha", "resumed": false,
            "intakes_reembedded": 3, "chunks_written": 120, "skipped_empty": [],
        })
    }

    #[test]
    fn a_reset_reply_reads_as_the_counts_and_the_restart() {
        assert_eq!(
            reset_sentence(&reset_reply(), "intake").as_deref(),
            Some(
                "Reset alpha: 3 intake(s) re-embedded, 120 chunk row(s) written.\n\
                 Restart the daemon so the new model takes effect."
            )
        );
    }

    #[test]
    fn a_reset_reply_names_the_items_it_skipped() {
        let mut reply = reset_reply();
        reply["skipped_empty"] = json!([7, 9]);
        let sentence = reset_sentence(&reply, "intake").expect("a sentence");
        assert!(
            sentence.contains("Skipped, with nothing to chunk: 7, 9."),
            "{sentence}"
        );
    }

    #[test]
    fn a_resume_with_nothing_waiting_says_so() {
        let reply = json!({
            "ok": true, "library": "alpha", "resumed": true,
            "intakes_reembedded": 0, "chunks_written": 0, "skipped_empty": [],
        });
        let sentence = reset_sentence(&reply, "paper intake").expect("a sentence");
        assert!(
            sentence.starts_with("Nothing to resume on alpha"),
            "{sentence}"
        );
        assert!(!sentence.contains("Restart"), "{sentence}");
    }

    #[test]
    fn a_reset_reply_without_a_count_has_no_sentence() {
        for field in [
            "library",
            "resumed",
            "intakes_reembedded",
            "chunks_written",
            "skipped_empty",
        ] {
            let mut reply = reset_reply();
            reply.as_object_mut().unwrap().remove(field);
            assert_eq!(reset_sentence(&reply, "intake"), None, "without {field}");
        }
    }
}
