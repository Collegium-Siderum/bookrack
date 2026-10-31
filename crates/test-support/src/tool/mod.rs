// SPDX-License-Identifier: Apache-2.0

//! The sandbox tool's library side: the isolation plan a command under
//! test runs with, and the tripwires that refuse it before it runs.
//!
//! The tool runs `bookrack` the way the integration suites do, through
//! [`Spawn`], so there is one definition of what an isolated child's
//! environment looks like and no second copy to drift.

pub mod argv;
pub mod tripwire;

use std::ffi::OsString;
use std::path::Path;

use bookrack_config::NO_DOTENV_ENV;

use crate::sandbox::Sandbox;
use crate::spawn::{EnvPlan, Spawn};

/// The embedder address a child gets when no stub is running: the
/// discard port, so the built-in `localhost:11434` default never reaches
/// a real Ollama on the host.
pub const NO_STUB_URL: &str = "http://127.0.0.1:9";

/// The plan the tool runs `bin` under against `host`.
///
/// The tool routes through the registry rather than a data root, points
/// the embedder at `stub_url` or the discard port, and switches dotenv
/// loading off unless `dotenv` says this run is the one that places a
/// `.env` in the sandbox's working directory. `parent` stands in for the
/// tool's own environment, the one the sweep works against.
pub fn plan_for(
    bin: &Path,
    host: &Sandbox,
    stub_url: Option<&str>,
    dotenv: bool,
    parent: impl Iterator<Item = (OsString, OsString)>,
) -> EnvPlan {
    let mut spawn = Spawn::for_tool(bin, host).ollama_url(stub_url.unwrap_or(NO_STUB_URL));
    if !dotenv {
        spawn = spawn.extra_env(NO_DOTENV_ENV, "1");
    }
    spawn.plan(parent)
}
