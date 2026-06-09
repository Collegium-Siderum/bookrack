// SPDX-License-Identifier: Apache-2.0

//! `tray.focus` control-plane method.
//!
//! Signals the GUI tray (if any) to raise and focus its window.
//! A second launch from a GUI entry routes through this method
//! instead of competing for the session lock. With no GUI attached,
//! the underlying `tokio::sync::Notify` simply has no waiter; the
//! method still returns `{ "ok": true }` so the contract stays stable
//! between CLI-only and GUI builds.

use serde_json::{Value, json};

use super::MethodContext;

/// Raise the GUI window if a tray is attached; otherwise no-op.
pub fn focus(ctx: &MethodContext) -> Value {
    ctx.tray_focus_signal.notify_one();
    json!({ "ok": true })
}
