// SPDX-License-Identifier: Apache-2.0

//! Per-source collectors. Each module writes one or more files into
//! the bundle staging directory.
//!
//! Collectors **never** mutate the live data root — they only read and
//! copy. A collector whose source is missing or empty writes an empty
//! file (or skips, when even the directory should be omitted) and
//! logs a `tracing::debug!`; only a hard IO or schema failure bubbles
//! up as a [`crate::DiagnoseError`].

pub mod catalog;
pub mod corpus;
pub mod crashes;
pub mod env;
pub mod logs;
pub mod vectors;
