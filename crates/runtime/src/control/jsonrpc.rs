// SPDX-License-Identifier: Apache-2.0

//! JSON-RPC 2.0 frame and error types.
//!
//! Frames are newline-delimited: each line is one valid JSON value
//! shaped as a request, response, or server-initiated notification.
//! The dispatcher in [`super::methods`] returns one of these enums for
//! every incoming line; the connection task in [`super::socket`]
//! writes the serialised form back with a trailing `\n`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 protocol marker. Every frame carries this verbatim.
pub const JSONRPC_VERSION: &str = "2.0";

/// Standard JSON-RPC error code for unparsable input.
pub const PARSE_ERROR: i32 = -32700;
/// Standard JSON-RPC error code for a request missing required fields.
pub const INVALID_REQUEST: i32 = -32600;
/// Standard JSON-RPC error code for an unknown method name.
pub const METHOD_NOT_FOUND: i32 = -32601;
/// Standard JSON-RPC error code for a parameter mismatch.
pub const INVALID_PARAMS: i32 = -32602;
/// Standard JSON-RPC error code for a handler-side failure.
pub const INTERNAL_ERROR: i32 = -32603;
/// bookrack-specific: another write command is in flight.
pub const BUSY: i32 = -32001;
/// bookrack-specific: runtime is not yet ready to serve this method.
#[allow(dead_code)]
pub const NOT_READY: i32 = -32002;
/// bookrack-specific: multi-library handler received a `library`
/// field that does not exist in the registry.
#[allow(dead_code)]
pub const INVALID_LIBRARY: i32 = -32010;
/// bookrack-specific: `ingest.cancel` referenced a job id no longer
/// in the queue document.
pub const JOB_NOT_FOUND: i32 = -32011;
/// bookrack-specific: a destructive RPC was issued without the
/// caller-side confirmation flag. The control plane never prompts
/// on behalf of the client, so any destructive method that exposes
/// a `yes` parameter must receive `yes = true` to proceed.
pub const CONFIRMATION_REQUIRED: i32 = -32012;
/// bookrack-specific: an execute leg referenced a `plan_id` that
/// the daemon does not know — either it was never registered, was
/// already consumed, or expired and was reaped.
#[allow(dead_code)]
pub const PLAN_NOT_FOUND: i32 = -32013;
/// bookrack-specific: a `plan_id` was registered for a different
/// method than the execute leg presenting it (e.g. a
/// `corpus.rebuild` plan submitted to `vectors.reembed`).
#[allow(dead_code)]
pub const PLAN_KIND_MISMATCH: i32 = -32014;
/// bookrack-specific: a `plan_id` was registered against a
/// different library than the one the execute leg is scoped to.
#[allow(dead_code)]
pub const PLAN_LIBRARY_MISMATCH: i32 = -32015;

/// One inbound JSON-RPC request.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// One outbound JSON-RPC response.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// JSON-RPC error payload.
#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// One server-initiated event notification.
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: NotificationParams,
}

impl Notification {
    /// Build an `event` notification carrying one channel/value pair.
    pub fn event(channel: impl Into<String>, value: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            method: "event",
            params: NotificationParams {
                channel: channel.into(),
                value,
                lag: false,
            },
        }
    }

    /// Build a `lag` marker for clients that should resync via
    /// `events.snapshot`. The `value` field carries `null`; the `lag`
    /// flag is what the client keys on.
    pub fn lag(channel: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            method: "event",
            params: NotificationParams {
                channel: channel.into(),
                value: Value::Null,
                lag: true,
            },
        }
    }
}

/// `event` notification body. `channel` names the stream, `value`
/// carries the per-channel payload, and `lag` signals a drop the
/// receiver missed.
#[derive(Debug, Clone, Serialize)]
pub struct NotificationParams {
    pub channel: String,
    pub value: Value,
    #[serde(skip_serializing_if = "is_false")]
    pub lag: bool,
}

fn is_false(b: &bool) -> bool {
    !b
}

/// Parse one wire line as a [`Request`]. Returns an
/// [`INVALID_REQUEST`] error when the body is JSON but not a request,
/// and a [`PARSE_ERROR`] when it is not valid JSON.
pub fn parse_request(line: &str) -> Result<Request, (Option<Value>, RpcError)> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(err) => {
            return Err((
                None,
                RpcError::new(PARSE_ERROR, format!("parse error: {err}")),
            ));
        }
    };
    let id = value.get("id").cloned();
    let request: Request = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(err) => {
            return Err((
                id,
                RpcError::new(INVALID_REQUEST, format!("invalid request: {err}")),
            ));
        }
    };
    if request.jsonrpc != JSONRPC_VERSION {
        return Err((
            id,
            RpcError::new(
                INVALID_REQUEST,
                format!("unsupported jsonrpc version: {}", request.jsonrpc),
            ),
        ));
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_round_trip() {
        let req = parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"daemon.version"}"#).unwrap();
        assert_eq!(req.method, "daemon.version");
        assert_eq!(req.id, Some(Value::from(1)));
    }

    #[test]
    fn parse_request_rejects_non_json() {
        let (id, err) = parse_request("not json").unwrap_err();
        assert!(id.is_none());
        assert_eq!(err.code, PARSE_ERROR);
    }

    #[test]
    fn parse_request_rejects_wrong_jsonrpc_version() {
        let (id, err) = parse_request(r#"{"jsonrpc":"1.0","method":"x"}"#).unwrap_err();
        assert!(id.is_none());
        assert_eq!(err.code, INVALID_REQUEST);
    }

    #[test]
    fn notification_event_serialises_with_channel_field() {
        let n = Notification::event("daemon.state", Value::from("idle"));
        let s = serde_json::to_string(&n).unwrap();
        assert!(s.contains(r#""method":"event""#));
        assert!(s.contains(r#""channel":"daemon.state""#));
        assert!(s.contains(r#""value":"idle""#));
        assert!(!s.contains(r#""lag""#));
    }

    #[test]
    fn notification_lag_carries_lag_flag() {
        let n = Notification::lag("daemon.state");
        let s = serde_json::to_string(&n).unwrap();
        assert!(s.contains(r#""lag":true"#));
    }
}
