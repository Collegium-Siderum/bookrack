// SPDX-License-Identifier: Apache-2.0

//! The shape of a metadata-status read.
//!
//! [`MetadataReport`] is what `library.show_metadata_audit` returns: the
//! base [`BookDetail`] augmented with the persisted audit verdict and the
//! current review status. It does not re-run the metadata audit —
//! re-running it is a CLI-only feature gated on the loaded rules and
//! profile, which neither the server nor an agent has a stake in
//! reproducing on every read.

use serde::Serialize;

use crate::dto::BookDetail;

/// The metadata-status read returned by
/// [`crate::reads::metadata::show_metadata_audit`].
#[derive(Debug, Clone, Serialize)]
pub struct MetadataReport {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Full bibliographic record for the book.
    pub book: BookDetail,
    /// The audit verdict the ingest pipeline stamped on the row, when
    /// one is recorded (`clean` / `needs_work` / ...).
    pub stored_verdict: Option<String>,
    /// The audit confidence stamped on the row (`high` / `medium` /
    /// `low`), when one is recorded.
    pub stored_confidence: Option<String>,
    /// The current review status (`pending` / `acknowledged` /
    /// `approved` / `rejected`), when a review row exists.
    pub review_status: Option<String>,
}

/// One row of a paginated metadata list — returned by both
/// [`crate::reads::metadata::list_metadata`] (unfiltered) and
/// [`crate::reads::metadata::list_pending_reviews`] (review queue).
#[derive(Debug, Clone, Serialize)]
pub struct MetadataListRow {
    /// Catalog intake id of the book.
    pub intake_id: i64,
    /// Best-effort title.
    pub title: Option<String>,
    /// Confidence the audit assigned (`high` / `medium` / `low`).
    pub confidence: Option<String>,
    /// Current review status (`pending` / `acknowledged` / ...).
    pub review_status: Option<String>,
}

/// Paginated result of a metadata listing — see [`MetadataListRow`].
#[derive(Debug, Clone, Serialize)]
pub struct MetadataListPage {
    /// Books in this page.
    pub rows: Vec<MetadataListRow>,
    /// Total number of books matching the filter, regardless of
    /// pagination.
    pub total: u64,
    /// True when this page does not cover the full result set.
    pub truncated: bool,
}
