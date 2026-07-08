// SPDX-License-Identifier: Apache-2.0

//! Per-command implementations. Each submodule owns one CLI command
//! and exposes a `run` entry point that the `main.rs` dispatch router
//! calls into. The shared loaders (`audit_helpers`, `embed_helpers`,
//! `ops_helpers`) and small utilities (`util`) live one level up so
//! commands depend on them through `crate::*`, not on each other.

pub mod audit_profile;
pub mod corpus;
pub mod diagnose;
pub mod dryrun;
pub mod index_profile;
pub mod ingest;
pub mod intake_ocr;
pub mod libraries;
pub mod metadata;
pub mod papers_corpus;
pub mod papers_dryrun;
pub mod papers_stamps;
pub mod papers_vectors;
pub mod remove;
pub mod remove_paper;
pub mod stamps;
pub mod vectors;
pub mod verify;
