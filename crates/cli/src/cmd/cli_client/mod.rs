//! `cli_client::*` — one-shot CLI subcommands implemented as thin
//! control-plane clients. Each module owns the dispatch for one
//! top-level `bookrack <subcommand>` invocation: connect, drive the
//! matching RPC, render the response, exit.
//!
//! Every module follows the same skeleton: discover the daemon via
//! [`helpers::connect`], optionally subscribe to the event
//! stream for progress, call the RPC, render the result. When the
//! daemon is not running the helpers return
//! [`crate::error::BookrackCliError::DaemonNotRunning`], which the
//! top-level reporter in `main` renders as a uniform one-line
//! "bookrack: …" message and maps to exit code 2.

pub mod corpus;
pub mod diagnose;
pub mod doctor;
pub mod dryrun;
pub mod helpers;
pub mod index_profile;
pub mod ingest;
pub mod intake;
pub mod libraries;
pub mod logs;
pub mod metadata;
pub mod papers;
pub mod queue;
pub mod quit;
pub mod remove;
pub mod stamps;
pub mod vectors;
pub mod verify;
