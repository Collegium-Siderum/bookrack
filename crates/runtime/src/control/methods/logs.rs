// SPDX-License-Identifier: Apache-2.0

//! `logs.tail` control-plane method.
//!
//! Returns the most recent N events from the daemon's in-memory ring
//! buffer (oldest first within the returned slice). The `n` a caller
//! omits and the ceiling one is clamped to are
//! [`bookrack_obs::stream::TAIL_REQUEST_DEFAULT`] and
//! [`bookrack_obs::stream::TAIL_REQUEST_MAX`], shared with the
//! `session.logs_tail` MCP tool over the same ring.

use bookrack_obs::stream::{TAIL_REQUEST_DEFAULT, TAIL_REQUEST_MAX};
use serde::Deserialize;
use serde_json::{Value, json};

use super::MethodContext;
use crate::control::jsonrpc::{INVALID_PARAMS, RpcError};

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
    let n = parsed
        .n
        .unwrap_or(TAIL_REQUEST_DEFAULT)
        .min(TAIL_REQUEST_MAX);
    let events = ctx.log_stream.tail(n);
    let returned = events.len();
    Ok(json!({ "events": events, "returned": returned }))
}
