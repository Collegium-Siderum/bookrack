// SPDX-License-Identifier: Apache-2.0

//! Snapshot the daemon's ingest-queue document (`queue.json`) through
//! the scrubber.

use std::path::Path;

use bookrack_core::queue::{
    QUEUE_SCHEMA_VERSION, QueueOpenDecision, QueueState, queue_open_decision,
};

use crate::Result;
use crate::scrub::Scrubber;

/// Write `<bundle>/queue/queue.json`: the queue document from the
/// daemon state directory, parsed and re-serialised so every path in
/// it passes through the scrubber. The document is daemon state rather
/// than library state — one per daemon process, however many
/// libraries it serves — so it is read from the state directory the
/// daemon resolves, not from the data root.
///
/// An absent document is the normal state of a daemon nothing has
/// been queued on and writes nothing. A document that cannot be read
/// or parsed, or that a newer binary wrote — the state the daemon
/// refuses to start on — writes `open-error.json` instead, naming the
/// reason; a state directory that cannot be resolved at all writes
/// the same file with the resolution failure.
pub fn collect(bundle_dir: &Path, scrubber: &Scrubber) -> Result<()> {
    let dst = bundle_dir.join("queue");
    let document = Path::new("queue.json");
    let dir = match bookrack_config::daemon_state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            std::fs::create_dir_all(&dst)?;
            return super::write_open_error(&dst, document, Some(&bookrack_core::error_chain(&e)));
        }
    };
    let path = dir.join(document);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            std::fs::create_dir_all(&dst)?;
            return super::write_open_error(&dst, document, Some(&e.to_string()));
        }
    };
    std::fs::create_dir_all(&dst)?;

    let state = match parse(&bytes) {
        Ok(state) => state,
        Err(reason) => {
            tracing::warn!(error = %reason, "diagnose: could not read the queue document");
            return super::write_open_error(&dst, document, Some(&reason));
        }
    };
    let mut value = serde_json::to_value(&state)?;
    scrubber.scrub_value(&mut value);
    let mut text = serde_json::to_string_pretty(&value)?;
    text.push('\n');
    std::fs::write(dst.join("queue.json"), text)?;
    Ok(())
}

/// Read the document the way the daemon does at start-up: the version
/// is judged before the shape, so a document from a newer binary is
/// reported as such rather than as malformed for carrying a field or
/// variant this build has no match for.
fn parse(bytes: &[u8]) -> std::result::Result<QueueState, String> {
    let probe: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("cannot parse the document: {e}"))?;
    let found = probe
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| "the document carries no schema_version".to_string())?;
    if queue_open_decision(found) == QueueOpenDecision::Refuse {
        return Err(format!(
            "written by schema version {found}; this build reads version \
             {QUEUE_SCHEMA_VERSION} and the daemon will not load it"
        ));
    }
    serde_json::from_slice(bytes).map_err(|e| format!("cannot parse the document: {e}"))
}
