// SPDX-License-Identifier: Apache-2.0

//! The paper audit's storage side: the projection of a report onto
//! the catalog's audit row. The judgement itself is
//! `bookrack_metadata::paper`.

pub mod projection;

pub use projection::paper_report_to_audit_row;
