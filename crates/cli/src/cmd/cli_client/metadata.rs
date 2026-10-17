//! `bookrack metadata {set,clear,void,reaudit,contributor,ack,approve,
//! reject,advance}` — route the matching write to the control-plane
//! method and word its reply.

use std::path::PathBuf;

use bookrack_cli_grammar::WriteMetadataAction;
use eyre::Result;
use serde_json::{Value, json};

use super::helpers;
use super::vectors::library_of;

pub async fn run(
    action: WriteMetadataAction,
    runtime_dir: Option<PathBuf>,
    default_audit_profile: Option<String>,
) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    let (method, params) = request(&action, default_audit_profile.as_deref());
    let reply = helpers::call_with_progress_value(client, method, params).await?;
    helpers::emit_outcome(&reply, sentence(&action, &reply));
    Ok(())
}

/// The control-plane method and params for one write. `audit_profile`
/// rides only on the two actions that run the audit.
fn request(action: &WriteMetadataAction, audit_profile: Option<&str>) -> (&'static str, Value) {
    let with_profile = |mut params: Value| {
        if let Some(name) = audit_profile {
            params["audit_profile"] = Value::String(name.to_string());
        }
        params
    };
    match action {
        WriteMetadataAction::Set {
            book,
            field,
            value,
            reason,
            confirmed,
        } => (
            "metadata.set",
            json!({
                "book": book,
                "field": field,
                "value": value,
                "reason": reason,
                "confirmed": confirmed,
            }),
        ),
        WriteMetadataAction::Clear {
            book,
            field,
            reason,
        } => (
            "metadata.clear",
            json!({"book": book, "field": field, "reason": reason}),
        ),
        WriteMetadataAction::Void {
            book,
            field,
            reason,
        } => (
            "metadata.void",
            json!({"book": book, "field": field, "reason": reason}),
        ),
        WriteMetadataAction::Reaudit { book } => {
            ("metadata.reaudit", with_profile(json!({ "book": book })))
        }
        WriteMetadataAction::ContributorAdd {
            book,
            role,
            name,
            nationality,
            reason,
        } => (
            "metadata.contributor_add",
            json!({
                "book": book,
                "role": role,
                "name": name,
                "nationality": nationality,
                "reason": reason,
            }),
        ),
        WriteMetadataAction::ContributorRemove {
            book,
            contributor_id,
            reason,
        } => (
            "metadata.contributor_remove",
            json!({"book": book, "contributor_id": contributor_id, "reason": reason}),
        ),
        WriteMetadataAction::Ack { book, reason } => {
            ("metadata.ack", json!({"book": book, "reason": reason}))
        }
        WriteMetadataAction::Approve { book, reason } => {
            ("metadata.approve", json!({"book": book, "reason": reason}))
        }
        WriteMetadataAction::Reject { book, reason } => {
            ("metadata.reject", json!({"book": book, "reason": reason}))
        }
        WriteMetadataAction::Advance { book } => {
            ("metadata.advance", with_profile(json!({ "book": book })))
        }
    }
}

/// The human-mode sentence for one write's reply, or `None` when the
/// reply lacks a field the sentence states. What is required depends
/// on the action: an edit states its audit row, a re-audit its verdict,
/// an advance its counts — and every sentence names the library.
fn sentence(action: &WriteMetadataAction, reply: &Value) -> Option<String> {
    let library = library_of(reply)?;
    let text = match action {
        WriteMetadataAction::Set { book, field, .. } => {
            edited(reply)?;
            format!("Set {field} on book {book}.")
        }
        WriteMetadataAction::Clear { book, field, .. } => {
            if edited(reply)? {
                format!("Cleared override on {field} for book {book}.")
            } else {
                format!("No override on {field} for book {book}; nothing to clear.")
            }
        }
        WriteMetadataAction::Void { book, field, .. } => {
            if edited(reply)? {
                format!("Voided {field} on book {book}; the field now reads as absent.")
            } else {
                format!(
                    "Voided {field} on book {book}; it had no effective value, \
                     the tombstone is recorded."
                )
            }
        }
        WriteMetadataAction::Reaudit { book } => {
            let verdict = reply.get("verdict")?.as_str()?;
            let confidence = reply.get("confidence")?.as_str()?;
            let previous = |key: &str| {
                reply
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("unset")
                    .to_string()
            };
            format!(
                "Reaudited book {book}: verdict {verdict} (was {}), confidence {confidence} \
                 (was {}).",
                previous("previous_verdict"),
                previous("previous_confidence"),
            )
        }
        WriteMetadataAction::ContributorAdd {
            book, role, name, ..
        } => {
            edited(reply)?;
            let id = reply.get("contributor_id")?.as_i64()?;
            format!("Added {role} {name:?} to book {book} (contributor id {id}).")
        }
        WriteMetadataAction::ContributorRemove {
            book,
            contributor_id,
            ..
        } => {
            edited(reply)?;
            format!("Removed contributor row {contributor_id} from book {book}.")
        }
        WriteMetadataAction::Ack { book, .. } => {
            edited(reply)?;
            format!("Acknowledged metadata gap on book {book}.")
        }
        WriteMetadataAction::Approve { book, .. } => {
            edited(reply)?;
            let approved = format!("Approved metadata on book {book}. Library: {library}.");
            return Some(match resumed_counts(reply) {
                Some((chunks, batches)) => format!(
                    "{approved}\nAdvanced book {book}: embedded {chunks} chunks across \
                     {batches} batches."
                ),
                None => approved,
            });
        }
        WriteMetadataAction::Reject { book, .. } => {
            edited(reply)?;
            format!("Rejected book {book}.")
        }
        WriteMetadataAction::Advance { book } => {
            let (chunks, batches) = counts(reply)?;
            format!("Advanced book {book}: embedded {chunks} chunks across {batches} batches.")
        }
    };
    Some(format!("{text} Library: {library}."))
}

/// `changed` from an edit's reply, provided the reply also names the
/// audit row the edit wrote.
fn edited(reply: &Value) -> Option<bool> {
    reply.get("audit_id")?.as_i64()?;
    reply.get("changed")?.as_bool()
}

/// The resume counts a reply carries.
fn counts(reply: &Value) -> Option<(u64, u64)> {
    Some((
        reply.get("chunks_written")?.as_u64()?,
        reply.get("batches")?.as_u64()?,
    ))
}

/// The resume counts of an approval that resumed the book.
fn resumed_counts(reply: &Value) -> Option<(u64, u64)> {
    if reply.get("resumed")?.as_bool()? {
        counts(reply)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit_reply(changed: bool) -> Value {
        json!({
            "ok": true, "library": "alpha", "book": 7, "audit_id": 41,
            "actor_kind": "user", "actor_detail": "cli", "changed": changed,
        })
    }

    fn set() -> WriteMetadataAction {
        WriteMetadataAction::Set {
            book: 7,
            field: "title".to_string(),
            value: "A Synthetic Ledger".to_string(),
            reason: None,
            confirmed: false,
        }
    }

    #[test]
    fn an_edit_reads_as_the_change_on_the_library() {
        assert_eq!(
            sentence(&set(), &edit_reply(true)).as_deref(),
            Some("Set title on book 7. Library: alpha.")
        );
    }

    #[test]
    fn a_clear_that_changed_nothing_says_so() {
        let clear = WriteMetadataAction::Clear {
            book: 7,
            field: "title".to_string(),
            reason: None,
        };
        assert_eq!(
            sentence(&clear, &edit_reply(false)).as_deref(),
            Some("No override on title for book 7; nothing to clear. Library: alpha.")
        );
    }

    #[test]
    fn an_edit_reply_without_its_audit_row_has_no_sentence() {
        let mut reply = edit_reply(true);
        reply.as_object_mut().unwrap().remove("audit_id");
        assert_eq!(sentence(&set(), &reply), None);
        assert_eq!(sentence(&set(), &json!({ "ok": true })), None);
    }

    #[test]
    fn an_edit_reply_without_the_library_has_no_sentence() {
        let mut reply = edit_reply(true);
        reply.as_object_mut().unwrap().remove("library");
        assert_eq!(sentence(&set(), &reply), None);
    }

    #[test]
    fn a_contributor_add_reads_as_the_new_row() {
        let add = WriteMetadataAction::ContributorAdd {
            book: 7,
            role: "author".to_string(),
            name: "Synthetic Author".to_string(),
            nationality: None,
            reason: None,
        };
        let mut reply = edit_reply(true);
        reply["contributor_id"] = json!(3);
        assert_eq!(
            sentence(&add, &reply).as_deref(),
            Some(
                "Added author \"Synthetic Author\" to book 7 (contributor id 3). \
                 Library: alpha."
            )
        );
    }

    /// A re-audit writes no audit row, so its sentence must not wait
    /// for one: it stands on the verdict and confidence.
    #[test]
    fn a_reaudit_reads_as_the_verdict_without_an_audit_row() {
        let reply = json!({
            "ok": true, "library": "alpha", "book": 7, "verdict": "plausible",
            "previous_verdict": null, "confidence": "high", "previous_confidence": "low",
        });
        assert_eq!(
            sentence(&WriteMetadataAction::Reaudit { book: 7 }, &reply).as_deref(),
            Some(
                "Reaudited book 7: verdict plausible (was unset), confidence high (was low). \
                 Library: alpha."
            )
        );
    }

    #[test]
    fn a_reaudit_reply_without_a_verdict_has_no_sentence() {
        let reply = json!({ "ok": true, "library": "alpha", "book": 7, "confidence": "high" });
        assert_eq!(
            sentence(&WriteMetadataAction::Reaudit { book: 7 }, &reply),
            None
        );
    }

    /// An advance writes no audit row either; its sentence stands on
    /// the counts.
    #[test]
    fn an_advance_reads_as_the_counts_without_an_audit_row() {
        let reply = json!({
            "ok": true, "library": "alpha", "book": 7, "resumed": true,
            "chunks_written": 12, "batches": 2,
        });
        assert_eq!(
            sentence(&WriteMetadataAction::Advance { book: 7 }, &reply).as_deref(),
            Some("Advanced book 7: embedded 12 chunks across 2 batches. Library: alpha.")
        );
    }

    #[test]
    fn an_approve_that_resumed_adds_the_counts() {
        let mut reply = edit_reply(true);
        reply["resumed"] = json!(true);
        reply["chunks_written"] = json!(12);
        reply["batches"] = json!(2);
        let approve = WriteMetadataAction::Approve {
            book: 7,
            reason: None,
        };
        assert_eq!(
            sentence(&approve, &reply).as_deref(),
            Some(
                "Approved metadata on book 7. Library: alpha.\n\
                 Advanced book 7: embedded 12 chunks across 2 batches."
            )
        );
    }

    #[test]
    fn an_approve_that_did_not_resume_is_the_edit_alone() {
        let mut reply = edit_reply(true);
        reply["resumed"] = json!(false);
        let approve = WriteMetadataAction::Approve {
            book: 7,
            reason: None,
        };
        assert_eq!(
            sentence(&approve, &reply).as_deref(),
            Some("Approved metadata on book 7. Library: alpha.")
        );
    }

    #[test]
    fn the_audit_profile_rides_only_on_the_audit_callers() {
        let (_, reaudit) = request(&WriteMetadataAction::Reaudit { book: 7 }, Some("strict"));
        assert_eq!(reaudit["audit_profile"], "strict");
        let (_, advance) = request(&WriteMetadataAction::Advance { book: 7 }, Some("strict"));
        assert_eq!(advance["audit_profile"], "strict");
        let (_, set) = request(&set(), Some("strict"));
        assert!(set.get("audit_profile").is_none(), "{set}");
    }
}
