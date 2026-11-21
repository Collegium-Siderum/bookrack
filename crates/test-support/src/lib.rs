// SPDX-License-Identifier: Apache-2.0

//! Host isolation for the integration suites.
//!
//! An integration test that spawns `bookrack`, or that brings a daemon
//! up in process, reads whatever the machine running it happens to
//! have: a real registry naming real libraries, a data root full of
//! books, a `.env` two directories up. The test then passes or fails
//! for reasons that have nothing to do with the code — and, worse,
//! passes on the maintainer's machine while the same commit fails on a
//! runner that has none of it.
//!
//! Answering that with a checklist does not hold: a suite where each
//! test remembers to redirect the environment leaks again the moment
//! someone writes the next test. This crate is built so that **not
//! isolating cannot be expressed**:
//!
//! - [`bookrack_cmd!`] and [`bookrack_bin!`] are the two ways to name the
//!   `bookrack` binary. The first returns a builder whose environment is
//!   already redirected; there is no raw constructor and no
//!   `isolate: bool`, every weakening is a named method, so the
//!   departures are greppable. The second returns a handle that only the
//!   sandbox tool ([`tool::Session`]) can run, inside a sandbox it built.
//! - [`process_env`] is the only way to redirect the test binary's own
//!   environment, and it installs one spec per process.
//! - `scripts/leak-check.sh` refuses a test file that names
//!   `CARGO_BIN_EXE_bookrack` outside those two macros, mutates the
//!   process environment by hand, or sets a `BOOKRACK_*` variable on a
//!   child.
//! - [`test_group!`] guards a grouped integration test binary: every
//!   sibling file is declared in its `main.rs`, and every file installs
//!   the one spec the group is built around.
//!
//! Both halves share one [`Sandbox`], so a test that runs a daemon in
//! process and a client as a child points them at the same tree by
//! construction rather than by two matching sets of assignments.
//!
//! A test that drives the child itself, rather than only its exit,
//! hands the built command to [`DaemonProcess`], which drains both
//! output streams while the child runs and judges its readiness and
//! exit.
//!
//! What this crate does **not** do is start from an empty environment:
//! it isolates relative to the parent process, keeping `PATH` and the
//! `CI` that turns a missing native library into a loud failure.
//! Starting from nothing is `scripts/test-clean.sh`.
//!
//! A sandbox carries no native library unless a test asks: the PDFium
//! build installed on the host is linked in by
//! [`Sandbox::with_host_pdfium`] and by the [`ProcessEnv::daemon`]
//! preset, and found there by the product's own search. The sandbox
//! tool's sandboxes never ask: one stands in for an operator's machine,
//! and installs the library the way an operator does.

mod child;
mod embed_stub;
pub mod fixtures;
mod host_slot;
mod process;
mod sandbox;
mod spawn;
pub mod test_group;
#[cfg(unix)]
pub mod tool;

#[cfg(unix)]
pub use bookrack_session::{endpoint_matches, endpoint_path_matches};
pub use child::{
    CaptureMode, DaemonProcess, Exit, Failure, FailureKind, Output, Ready, ReadyFailure, Sink,
    SpawnOpts, Stage, StreamStats, TEARDOWN_BUDGET,
};
pub use embed_stub::{EmbedFailure, EmbedStub, StubSpec, StubSpecError, Vectors};
pub use host_slot::host_pdfium_root;
pub use process::{ProcessEnv, process_env};
pub use sandbox::Sandbox;
pub use spawn::{EnvPlan, Spawn};
