// SPDX-License-Identifier: Apache-2.0

//! `logs.tail` control-plane method.
//!
//! Returns the most recent N events from the daemon's in-memory ring
//! buffer (oldest first within the returned slice). Mirrors the
//! `session.logs_tail` MCP tool: defaults to 100, capped at 1024.

use serde::Deserialize;
use serde_json::{Value, json};

use super::MethodContext;
use crate::control::jsonrpc::{INVALID_PARAMS, RpcError};

/// Default `n` when the caller omits it.
pub const TAIL_DEFAULT: usize = 100;

/// Server-side cap on `n`.
pub const TAIL_MAX: usize = 1024;

#[derive(Debug, Deserialize, Default)]
struct TailParams {
    #[serde(default)]
    n: Option<usize>,
}

pub fn tail(params: &Option<Value>, ctx: &MethodContext) -> Result<Value, RpcError> {
    let parsed: TailParams = match params {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("invalid logs.tail params: {e}")))?,
        None => TailParams::default(),
    };
    let n = parsed.n.unwrap_or(TAIL_DEFAULT).min(TAIL_MAX);
    let events = ctx.log_stream.tail(n);
    let returned = events.len();
    Ok(json!({ "events": events, "returned": returned }))
}
