// SPDX-License-Identifier: Apache-2.0

//! `bookrack logs` — stream or snapshot the running daemon's log
//! events.
//!
//! Two leg of the same surface:
//! * `--tail N` — snapshot the last N events via the `logs.tail`
//!   read RPC. Capped server-side at 1024.
//! * `--follow` — subscribe to the broadcast and stream new `log`
//!   events as they arrive. Implicit when no other flag is set.
//!
//! Combine both for `tail | follow` semantics: the snapshot is
//! emitted first, then the live stream takes over.
//!
//! Human mode renders each event as
//! `HH:MM:SS LEVEL target | message`. `--json` emits the underlying
//! `LogEvent` payload one record per line so a pipe consumer can
//! `jq -c` over it.

use std::path::PathBuf;

use bookrack_cli::render::ctx;
use bookrack_cli_grammar::LogsArgs;
use bookrack_control_client::Event;
use bookrack_obs::stream::LogEvent;
use eyre::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::helpers;

pub async fn run(args: LogsArgs, runtime_dir: Option<PathBuf>) -> Result<()> {
    let level_floor = parse_level_floor(args.level.as_deref())?;
    let client = helpers::connect(runtime_dir.as_deref()).await?;

    // When `--follow` (explicit or implicit) is on, subscribe before
    // the snapshot RPC so an event fired between the two does not
    // slip past the broadcast.
    let want_follow = args.follow || args.tail.is_none();
    let live_rx = if want_follow {
        Some(
            client
                .subscribe()
                .await
                .context("subscribe to control-plane events")?,
        )
    } else {
        None
    };

    if let Some(n) = args.tail {
        let response = helpers::dispatch(&client, "logs.tail", json!({ "n": n })).await?;
        if let Some(events) = response.get("events").and_then(Value::as_array) {
            for raw in events {
                if let Ok(ev) = serde_json::from_value::<LogEvent>(raw.clone()) {
                    emit_event(&ev, level_floor);
                }
            }
        }
    }

    if let Some(mut rx) = live_rx {
        follow(&mut rx, level_floor).await?;
    }
    Ok(())
}

async fn follow(rx: &mut broadcast::Receiver<Event>, level_floor: Option<u8>) -> Result<()> {
    loop {
        match rx.recv().await {
            Ok(event) if event.channel == "log" => {
                if let Ok(ev) = serde_json::from_value::<LogEvent>(event.value) {
                    emit_event(&ev, level_floor);
                }
            }
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                eprintln!("bookrack: log stream lagged; some events were dropped");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

pub(crate) fn emit_event(ev: &LogEvent, level_floor: Option<u8>) {
    if let Some(floor) = level_floor {
        let rank = level_rank(&ev.level).unwrap_or(0);
        if rank < floor {
            return;
        }
    }
    if ctx().is_quiet() {
        return;
    }
    if ctx().is_json() {
        match serde_json::to_string(ev) {
            Ok(line) => println!("{line}"),
            Err(_) => println!("{{}}"),
        }
        return;
    }
    println!("{}", format_human_line(ev));
}

fn format_human_line(ev: &LogEvent) -> String {
    let ts = ev.ts.format("%H:%M:%S");
    let level = pad_level(&ev.level);
    let target = &ev.target;
    let message = ev.message.trim_end();
    format!("{ts} {level} {target} | {message}")
}

fn pad_level(level: &str) -> String {
    // Align so the column does not jitter between INFO / WARN / ERROR
    // (4) and DEBUG / TRACE (5).
    format!("{level:<5}")
}

/// Maps a tracing level to its severity rank, ignoring case.
fn level_rank(level: &str) -> Option<u8> {
    match level.to_ascii_uppercase().as_str() {
        "TRACE" => Some(1),
        "DEBUG" => Some(2),
        "INFO" => Some(3),
        "WARN" => Some(4),
        "ERROR" => Some(5),
        _ => None,
    }
}

fn parse_level_floor(level: Option<&str>) -> Result<Option<u8>> {
    let Some(raw) = level else { return Ok(None) };
    level_rank(raw).map(Some).ok_or_else(|| {
        eyre::eyre!("unknown --level value `{raw}`; expected TRACE/DEBUG/INFO/WARN/ERROR")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn ev(level: &str, target: &str, message: &str) -> LogEvent {
        LogEvent {
            ts: Utc.with_ymd_and_hms(2026, 6, 26, 12, 34, 56).unwrap(),
            level: level.to_string(),
            target: target.to_string(),
            message: message.to_string(),
            fields: Default::default(),
        }
    }

    #[test]
    fn human_line_uses_hms_and_pads_level() {
        let line = format_human_line(&ev("INFO", "bookrack_ingest", "starting"));
        assert!(line.starts_with("12:34:56 INFO  bookrack_ingest | "));
        assert!(line.ends_with("starting"));
    }

    #[test]
    fn level_rank_is_case_insensitive() {
        assert_eq!(level_rank("info"), Some(3));
        assert_eq!(level_rank("INFO"), Some(3));
        assert_eq!(level_rank("Warn"), Some(4));
        assert_eq!(level_rank("nope"), None);
    }

    #[test]
    fn parse_level_floor_rejects_unknown() {
        assert!(parse_level_floor(Some("BOGUS")).is_err());
        assert_eq!(parse_level_floor(Some("info")).unwrap(), Some(3));
        assert_eq!(parse_level_floor(None).unwrap(), None);
    }
}
