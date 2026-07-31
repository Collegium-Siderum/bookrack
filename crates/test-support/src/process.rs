// SPDX-License-Identifier: Apache-2.0

//! Redirecting the *test binary's own* environment into a sandbox.
//!
//! The tests that build a [`bookrack_runtime::DaemonRuntime`] in
//! process cannot be isolated by a child's environment: they read the
//! host through this process. [`process_env`] is the one implementation
//! of that redirection, replacing the per-file `OnceLock<TempDir>`
//! copies that each isolated a different subset.
//!
//! Unlike [`crate::Spawn`], this side does **not** move the working
//! directory: in-process tests locate their fixtures relative to
//! `CARGO_MANIFEST_DIR`, and a process-wide `chdir` is an action at a
//! distance. The `.env` that a stable working directory would have shut
//! out is handled by a pre-flight assertion instead.

use std::path::PathBuf;
use std::sync::OnceLock;

use bookrack_config::{DAEMON_STATE_DIR_ENV, DATA_DIR_ENV, OLLAMA_URL_ENV, REGISTRY_ENV};
use bookrack_session::RUNTIME_DIR_ENV;

use crate::embed_stub::EmbedStub;
use crate::sandbox::Sandbox;
use crate::spawn::PASSTHROUGH_ENV;

const BOOKRACK_PREFIX: &str = "BOOKRACK_";

static ENV: OnceLock<(ProcessEnv, Sandbox)> = OnceLock::new();

/// What a test binary needs its own environment to look like.
///
/// Start from a preset and weaken it with the named methods, the same
/// discipline [`crate::Spawn`] follows: there is no way to ask for less
/// isolation without writing down which part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEnv {
    embedder: bool,
    data_dir: bool,
    registry: RegistrySpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistrySpec {
    /// The sandbox's own registry file, which exists and is empty.
    Sandbox,
    /// A path inside the sandbox that is deliberately never created,
    /// for the tests that assert something else creates it.
    Absent(String),
}

impl ProcessEnv {
    /// Home, data root, registry, runtime directory, and daemon state
    /// directory all inside a sandbox. No embedder: the crates that
    /// never open a library do not need one.
    pub fn isolated() -> ProcessEnv {
        ProcessEnv {
            embedder: false,
            data_dir: true,
            registry: RegistrySpec::Sandbox,
        }
    }

    /// [`ProcessEnv::isolated`] plus a loopback embedder, for the
    /// binaries that bring a daemon up: every library open probes the
    /// configured embedder for its vector width.
    pub fn daemon() -> ProcessEnv {
        ProcessEnv {
            embedder: true,
            ..ProcessEnv::isolated()
        }
    }

    /// Leave the data root unset, so resolution goes through the
    /// registry.
    pub fn without_data_dir(mut self) -> ProcessEnv {
        self.data_dir = false;
        self
    }

    /// Name a registry file inside the sandbox that is not created, so
    /// a test can assert the code under test creates it.
    pub fn registry_absent(mut self, name: &str) -> ProcessEnv {
        self.registry = RegistrySpec::Absent(name.to_string());
        self
    }
}

/// Redirect this process's environment into a sandbox once, and return
/// the sandbox every later caller shares.
///
/// Call it as the first statement of every test in the binary. The
/// sandbox outlives the process, so a `&'static Sandbox` can be handed
/// straight to [`bookrack_cmd!`](crate::bookrack_cmd) — parent and
/// child then read one tree by construction rather than by two matching
/// sets of assignments.
///
/// Panics when a second caller in the same binary asks for a different
/// [`ProcessEnv`]: initialisation happens once, so the second request
/// would otherwise be discarded in silence.
pub fn process_env(spec: ProcessEnv) -> &'static Sandbox {
    assert_no_hostile_dotenv();
    let (stored, sandbox) = ENV.get_or_init(|| {
        let sandbox = Sandbox::new();
        apply(&spec, &sandbox);
        (spec.clone(), sandbox)
    });
    assert_eq!(
        *stored, spec,
        "this binary already installed a different process environment; \
         one binary gets one spec, and the second is discarded",
    );
    sandbox
}

/// Install `spec` into this process's environment.
fn apply(spec: &ProcessEnv, sandbox: &Sandbox) {
    let mut vars: Vec<(&str, PathBuf)> = vec![
        ("HOME", sandbox.home()),
        ("XDG_CONFIG_HOME", sandbox.config_home()),
        ("XDG_DATA_HOME", sandbox.data_home()),
        ("XDG_CACHE_HOME", sandbox.cache_home()),
        (RUNTIME_DIR_ENV, sandbox.runtime_dir()),
        (DAEMON_STATE_DIR_ENV, sandbox.daemon_state_dir()),
    ];
    if spec.data_dir {
        vars.push((DATA_DIR_ENV, sandbox.data_dir()));
    }
    match &spec.registry {
        RegistrySpec::Sandbox => vars.push((REGISTRY_ENV, sandbox.registry_path())),
        RegistrySpec::Absent(name) => vars.push((REGISTRY_ENV, sandbox.path().join(name))),
    }

    let mut installed: Vec<&str> = vars.iter().map(|(name, _)| *name).collect();
    installed.extend(PASSTHROUGH_ENV);
    if spec.embedder {
        installed.push(OLLAMA_URL_ENV);
    }
    let swept: Vec<std::ffi::OsString> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            key.to_str()
                .is_some_and(|k| k.starts_with(BOOKRACK_PREFIX) && !installed.contains(&k))
        })
        .collect();

    // SAFETY: the environment is mutated exactly once per process,
    // inside `OnceLock::get_or_init`'s single-initialization guarantee,
    // as the first statement of every test in the binary and therefore
    // before any concurrent reader exists. `cargo nextest` gives each
    // test its own process, which is what makes "first statement of
    // every test" enough.
    unsafe {
        for key in swept {
            std::env::remove_var(key);
        }
        for (key, value) in vars {
            std::env::set_var(key, value);
        }
        if spec.embedder {
            std::env::set_var(OLLAMA_URL_ENV, EmbedStub::url());
        }
    }
}

/// Fail loudly when a `.env` reachable from this process's working
/// directory defines a bookrack variable.
///
/// `dotenvy` searches upward from the working directory, and cargo runs
/// a test binary from its package root, so such a file silently re-sets
/// whatever this module just removed — and, because `dotenvy` does not
/// overwrite variables that are already set, it does so only for the
/// ones a test took care to remove. Naming the file and the key turns
/// an invisible leak into a failure with a fix in it.
fn assert_no_hostile_dotenv() {
    let Ok(start) = std::env::current_dir() else {
        return;
    };
    for dir in start.ancestors() {
        let candidate = dir.join(".env");
        if !candidate.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&candidate) else {
            return;
        };
        let offenders: Vec<&str> = text
            .lines()
            .filter_map(dotenv_key)
            .filter(|key| key.starts_with(BOOKRACK_PREFIX) && !PASSTHROUGH_ENV.contains(key))
            .collect();
        assert!(
            offenders.is_empty(),
            "{} defines {:?}, which reaches this test through dotenv's \
             upward search and overrides the isolated environment",
            candidate.display(),
            offenders,
        );
        return;
    }
}

/// The variable name one `.env` line assigns, if it assigns one. Values
/// are deliberately not read: the check is about which names can reach
/// the process, and a value may be a secret.
fn dotenv_key(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line);
    let (key, _) = line.split_once('=')?;
    Some(key.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dotenv_key_is_read_without_its_value() {
        assert_eq!(
            dotenv_key("BOOKRACK_DATA_DIR=/secret"),
            Some("BOOKRACK_DATA_DIR")
        );
        assert_eq!(
            dotenv_key("export BOOKRACK_LOG=debug"),
            Some("BOOKRACK_LOG")
        );
        assert_eq!(dotenv_key("  KEY = value "), Some("KEY"));
        assert_eq!(dotenv_key("# BOOKRACK_DATA_DIR=/x"), None);
        assert_eq!(dotenv_key(""), None);
        assert_eq!(dotenv_key("no-assignment"), None);
    }

    /// The presets differ in exactly one bit, and the deviations are
    /// distinguishable — the equality the `OnceLock` guard compares has
    /// to be able to tell them apart.
    #[test]
    fn the_presets_and_their_deviations_are_distinct() {
        assert_ne!(ProcessEnv::isolated(), ProcessEnv::daemon());
        assert_ne!(
            ProcessEnv::daemon(),
            ProcessEnv::daemon().without_data_dir()
        );
        assert_ne!(
            ProcessEnv::isolated(),
            ProcessEnv::isolated().registry_absent("pinned-registry.toml")
        );
    }
}
