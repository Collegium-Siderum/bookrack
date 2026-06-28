// SPDX-License-Identifier: Apache-2.0

//! Paper-side metadata audit.
//!
//! Peer of `bookrack-metadata`'s `audit` function for the papers
//! pipeline. The two pipelines share generic grade types
//! (`FieldGrade`, `Verdict`, `Confidence`) and the storage stack but
//! keep their signal sets, profile schemas, and runtime data lists
//! independent so a change in one cannot quietly reach the other.
//!
//! Two on-disk schemas sit side by side under
//! `<data_root>/audit-rules/`, mirroring the books pipeline's layout:
//!
//! - [`PaperAuditProfile`] — toggles and numeric thresholds. Reads
//!   `paper_audit_profile.toml` plus an optional
//!   `paper_audit_profile.local.toml` overlay.
//! - [`PaperAuditData`] — runtime-loaded data lists. Reads
//!   `paper_audit_data.toml` plus an optional
//!   `paper_audit_data.local.toml` overlay.
//!
//! Three named built-in profiles — `default`, `trust-source`,
//! `strict` — match the books pipeline's preset shape.

pub mod csl_required;
pub mod data;
pub mod profile;
pub mod projection;
pub mod report;
pub mod signals;

pub use csl_required::{RequirementLevel, requirement};
pub use data::{DATA_OVERLAY_FILE, DEFAULT_DATA_TOML, DataLoadError, PaperAuditData};
pub use profile::{
    ALL_BUILT_IN_NAMES, DEFAULT_PROFILE_TOML, IdentifierToggles, LoadError as ProfileLoadError,
    PROFILE_DEFAULT, PROFILE_OVERLAY_FILE, PROFILE_STRICT, PROFILE_TRUST_SOURCE, PaperAuditProfile,
    SCHEMA_VERSION,
};
pub use projection::paper_report_to_audit_row;
pub use report::{
    PaperConfidence, PaperFieldGrade, PaperFieldReport, PaperFlag, PaperReport, PaperVerdict,
};
pub use signals::{PaperAuditInput, audit_paper, issn_checksum_ok, orcid_checksum_ok};
