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
//! - [`bookrack_cmd!`] is the only way to name the `bookrack` binary,
//!   and it returns a builder whose environment is already redirected.
//!   There is no raw constructor and no `isolate: bool`; every
//!   weakening is a named method, so the departures are greppable.
//! - [`process_env`] is the only way to redirect the test binary's own
//!   environment, and it installs one spec per process.
//! - `scripts/leak-check.sh` refuses a test file that names
//!   `CARGO_BIN_EXE_bookrack`, mutates the process environment by hand,
//!   or sets a `BOOKRACK_*` variable on a child.
//!
//! Both halves share one [`Sandbox`], so a test that runs a daemon in
//! process and a client as a child points them at the same tree by
//! construction rather than by two matching sets of assignments.
//!
//! What this crate does **not** do is start from an empty environment:
//! it isolates relative to the parent process, keeping `PATH`, `CI`,
//! and the PDFium pointer that turn a missing native library into a
//! loud failure. Starting from nothing is `scripts/test-clean.sh`.

mod embed_stub;
mod process;
mod sandbox;
mod spawn;

pub use embed_stub::{EmbedFailure, EmbedStub};
pub use process::{ProcessEnv, process_env};
pub use sandbox::Sandbox;
pub use spawn::{PASSTHROUGH_ENV, Spawn};
