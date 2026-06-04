// SPDX-License-Identifier: Apache-2.0

//! Write-side request and response DTOs.
//!
//! The full set lands in a later phase; this module exists so downstream
//! crates can already import the path and the [`WriteOutcome`] shape is
//! pinned.

use serde::Serialize;

/// What a write op records about the change it just made.
///
/// Every write op returns one of these so the caller can render or log
/// the resulting audit identity without a second round-trip.
#[derive(Debug, Clone, Serialize)]
pub struct WriteOutcome {
    /// Surrogate id of the `metadata_audit` row this op appended.
    pub audit_id: i64,
    /// Database string for the actor kind that performed the edit.
    pub actor_kind: String,
    /// Free-form actor identifier ("cli", "mcp", ...).
    pub actor_detail: Option<String>,
}
