// SPDX-License-Identifier: Apache-2.0

//! Metadata audit — the consultative quality check that grades a
//! book's bibliographic record without gating the pipeline.
//!
//! [`audit`] is a pure function. Its inputs are the extracted
//! [`bookrack_extract::Biblio`] and [`bookrack_extract::Provenance`],
//! the catalog's effective field values (base + overrides), the
//! warning-level TOC statistics computed during STRUCTURE, a short
//! body sample, and a bare source filename. Its output is a per-field
//! grade with structured flags, an aggregate verdict that maps onto
//! `node_reviews.status`, a row-level confidence that maps onto
//! `node_publication_attrs.confidence`, and the block indices that
//! may contain a copyright page.
//!
//! The audit never writes to the database, never reads from disk, and
//! never performs network I/O. Callers feed it inputs and persist
//! its output as they see fit.

mod filename;
mod publishers;
mod report;
mod rules;
mod signals;

pub use filename::{FilenameBiblio, parse as parse_filename};
pub use report::{
    AuditInput, Confidence, FieldGrade, FieldReport, Flag, MetadataReport, TocStats, Verdict,
};
pub use rules::{AuditRules, LoadError as RulesLoadError};
pub use signals::{is_valid_isbn, looks_like_timestamp};

/// Run the audit over one prepared input set.
pub fn audit(input: &AuditInput) -> MetadataReport {
    signals::run(input)
}
