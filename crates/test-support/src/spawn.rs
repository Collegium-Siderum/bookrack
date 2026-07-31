// SPDX-License-Identifier: Apache-2.0

//! Building an isolated `bookrack` child process.
//!
//! [`bookrack_cmd!`] is the only way to name the binary, and it hands
//! back a [`Spawn`] whose environment is already redirected into a
//! [`Sandbox`]. There is no constructor that skips that step: every
//! weakening of the isolation is a named method, so `git grep` over the
//! method names lists every place a test departs from the default.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use bookrack_config::{DAEMON_STATE_DIR_ENV, DATA_DIR_ENV, OLLAMA_URL_ENV, REGISTRY_ENV};
use bookrack_session::RUNTIME_DIR_ENV;

use crate::sandbox::Sandbox;

/// Prefix every variable this project reads shares.
const BOOKRACK_PREFIX: &str = "BOOKRACK_";

/// Variables the sweep lets through from the parent process.
///
/// Both name the PDFium binary the extraction tests bind at runtime.
/// CI exports them, and `bookrack_extract::pdfium_gate` turns a missing
/// library into a loud failure only while `BOOKRACK_REQUIRE_PDFIUM`
/// (or `CI`) says the environment was meant to carry it — sweeping them
/// away would turn that loud failure back into a silent skip. The
/// second name is spelled out rather than imported because
/// `bookrack-extract` pulls the whole PDF stack in with it.
pub const PASSTHROUGH_ENV: [&str; 2] = ["BOOKRACK_PDFIUM_LIB", "BOOKRACK_REQUIRE_PDFIUM"];

/// Name the `bookrack` binary under test and start an isolated
/// [`Spawn`] against `$sandbox` (anything that derefs to a
/// [`Sandbox`]).
///
/// `CARGO_BIN_EXE_bookrack` is defined only while cargo compiles a
/// test target of the crate that declares the binary, so the lookup
/// cannot live in a function here; the macro puts it in the caller's
/// crate, where it resolves.
#[macro_export]
macro_rules! bookrack_cmd {
    ($sandbox:expr) => {
        $crate::Spawn::__with_bin(env!("CARGO_BIN_EXE_bookrack"), $sandbox)
    };
}

/// A `bookrack` invocation whose every host-visible location points
/// into a sandbox.
///
/// Build it with [`bookrack_cmd!`], weaken it with the named methods,
/// then call [`Spawn::build`] for a [`std::process::Command`]. Async
/// call sites convert with `tokio::process::Command::from`, which keeps
/// this crate free of a tokio dependency.
pub struct Spawn {
    bin: PathBuf,
    home: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    cache_home: PathBuf,
    cwd: PathBuf,
    data_dir: Option<PathBuf>,
    registry: Option<PathBuf>,
    runtime_dir: PathBuf,
    daemon_state_dir: PathBuf,
    ollama_url: Option<String>,
    extra: Vec<(OsString, OsString)>,
    stdin_pipe: bool,
}

impl Spawn {
    /// Implementation detail of [`bookrack_cmd!`]. Not an entry point:
    /// call the macro, which supplies the binary path the caller's
    /// crate knows.
    #[doc(hidden)]
    pub fn __with_bin(bin: impl AsRef<Path>, sandbox: &Sandbox) -> Spawn {
        Spawn {
            bin: bin.as_ref().to_path_buf(),
            home: sandbox.home(),
            config_home: sandbox.config_home(),
            data_home: sandbox.data_home(),
            cache_home: sandbox.cache_home(),
            cwd: sandbox.cwd(),
            data_dir: Some(sandbox.data_dir()),
            registry: Some(sandbox.registry_path()),
            runtime_dir: sandbox.runtime_dir(),
            daemon_state_dir: sandbox.daemon_state_dir(),
            ollama_url: None,
            extra: Vec::new(),
            stdin_pipe: false,
        }
    }

    /// Leave the data root unset, so the child resolves one through the
    /// registry or reports that none is configured.
    pub fn without_data_dir(mut self) -> Spawn {
        self.data_dir = None;
        self
    }

    /// Point the data root somewhere other than the sandbox default.
    pub fn data_dir(mut self, path: impl AsRef<Path>) -> Spawn {
        self.data_dir = Some(path.as_ref().to_path_buf());
        self
    }

    /// Point the registry at `path` instead of the sandbox's own.
    pub fn registry(mut self, path: impl AsRef<Path>) -> Spawn {
        self.registry = Some(path.as_ref().to_path_buf());
        self
    }

    /// Leave the registry unset, so the child falls through to the
    /// platform-default path — which the redirected home keeps inside
    /// the sandbox, where no file exists.
    pub fn without_registry(mut self) -> Spawn {
        self.registry = None;
        self
    }

    /// Point the session runtime directory somewhere other than the
    /// sandbox default.
    pub fn runtime_dir(mut self, path: impl AsRef<Path>) -> Spawn {
        self.runtime_dir = path.as_ref().to_path_buf();
        self
    }

    /// Point the child's embedder at `url`, normally
    /// [`crate::EmbedStub::url`].
    pub fn ollama_url(mut self, url: impl Into<String>) -> Spawn {
        self.ollama_url = Some(url.into());
        self
    }

    /// Give the child a piped stdin, for the prompts a confirmation
    /// path reads.
    pub fn stdin_pipe(mut self) -> Spawn {
        self.stdin_pipe = true;
        self
    }

    /// Set one more variable on the child. It is exempt from the sweep
    /// like every other variable the builder sets.
    pub fn extra_env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Spawn {
        self.extra
            .push((key.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// Assemble the command.
    ///
    /// Three rules carry the isolation:
    ///
    /// 1. **Sweep, do not list.** Every `BOOKRACK_*` variable the
    ///    parent carries that this builder did not set and
    ///    [`PASSTHROUGH_ENV`] does not name is removed from the child.
    ///    A fixed override list would already miss the backup
    ///    directory, the reranker URL, the llama-server path, the log
    ///    directives, and the vector and search knobs — and the set of
    ///    names keeps growing, so a list rots by construction.
    /// 2. **No `env_clear`.** Clearing everything would drop `PATH`,
    ///    `TMPDIR`, `CI`, and `RUST_BACKTRACE` along with the hostile
    ///    variables; losing `CI` and the PDFium pointer turns the PDF
    ///    tests from a loud failure into a silent skip. What this
    ///    builder promises is isolation *relative to the parent*;
    ///    starting from an empty environment is `scripts/test-clean.sh`.
    /// 3. **A working directory inside the sandbox.** `dotenvy`
    ///    searches upward from the working directory, so a child that
    ///    inherited the test binary's would read the repository's
    ///    `.env` and re-set variables this builder just removed.
    pub fn build(self) -> Command {
        self.build_from(std::env::vars_os())
    }

    /// Pure form of [`Spawn::build`]: `host` stands in for the parent
    /// process's variables, so the sweep can be tested against a host
    /// shape without mutating this process's environment.
    fn build_from(self, host: impl Iterator<Item = (OsString, OsString)>) -> Command {
        let mut vars: Vec<(OsString, OsString)> = vec![
            ("HOME".into(), self.home.into()),
            ("XDG_CONFIG_HOME".into(), self.config_home.into()),
            ("XDG_DATA_HOME".into(), self.data_home.into()),
            ("XDG_CACHE_HOME".into(), self.cache_home.into()),
            (RUNTIME_DIR_ENV.into(), self.runtime_dir.into()),
            (DAEMON_STATE_DIR_ENV.into(), self.daemon_state_dir.into()),
        ];
        if let Some(path) = self.data_dir {
            vars.push((DATA_DIR_ENV.into(), path.into()));
        }
        if let Some(path) = self.registry {
            vars.push((REGISTRY_ENV.into(), path.into()));
        }
        if let Some(url) = self.ollama_url {
            vars.push((OLLAMA_URL_ENV.into(), url.into()));
        }
        vars.extend(self.extra);

        let assigned: HashSet<OsString> = vars.iter().map(|(key, _)| key.clone()).collect();
        let mut cmd = Command::new(&self.bin);
        for (key, _) in host {
            let swept = key
                .to_str()
                .is_some_and(|k| k.starts_with(BOOKRACK_PREFIX) && !PASSTHROUGH_ENV.contains(&k));
            if swept && !assigned.contains(&key) {
                cmd.env_remove(&key);
            }
        }
        for (key, value) in vars {
            cmd.env(key, value);
        }
        cmd.current_dir(self.cwd);
        if self.stdin_pipe {
            cmd.stdin(Stdio::piped());
        }
        cmd
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    use super::*;

    /// The builder's view of one command's environment: a variable the
    /// builder set maps to `Some(value)`, one it swept to `None`, and
    /// one it never touched is absent.
    fn envs(cmd: &Command) -> BTreeMap<String, Option<String>> {
        cmd.get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn spawn(sandbox: &Sandbox) -> Spawn {
        Spawn::__with_bin("/nonexistent/bookrack", sandbox)
    }

    /// A parent process carrying the hostile shape this crate exists to
    /// cut off, including two names the builder has never heard of.
    fn hostile_host() -> impl Iterator<Item = (OsString, OsString)> {
        [
            ("HOME", "/host/home"),
            ("PATH", "/usr/bin"),
            (DATA_DIR_ENV, "/host/library"),
            (REGISTRY_ENV, "/host/registry.toml"),
            (RUNTIME_DIR_ENV, "/host/runtime"),
            (DAEMON_STATE_DIR_ENV, "/host/state"),
            (OLLAMA_URL_ENV, "http://host:11434"),
            ("BOOKRACK_BACKUP_DIR", "/host/backup"),
            ("BOOKRACK_RERANKER_URL", "http://host:8080"),
            ("BOOKRACK_PDFIUM_LIB", "/host/pdfium"),
            ("BOOKRACK_REQUIRE_PDFIUM", "1"),
        ]
        .into_iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
    }

    /// Every location the builder knows about points inside the
    /// sandbox. Deleting one of the assignments in `build_from` drops
    /// that variable to the host's value.
    #[test]
    fn the_builder_overrides_every_hostile_variable_it_knows() {
        let sandbox = Sandbox::new();
        let cmd = spawn(&sandbox)
            .ollama_url("http://127.0.0.1:1")
            .build_from(hostile_host());
        let envs = envs(&cmd);
        let root = sandbox.path().to_string_lossy().into_owned();
        for key in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            DATA_DIR_ENV,
            REGISTRY_ENV,
            RUNTIME_DIR_ENV,
            DAEMON_STATE_DIR_ENV,
        ] {
            let value = envs
                .get(key)
                .unwrap_or_else(|| panic!("{key} is not set on the child"))
                .as_ref()
                .unwrap_or_else(|| panic!("{key} is removed rather than set"));
            assert!(
                value.starts_with(&root),
                "{key} = {value} escapes the sandbox at {root}",
            );
        }
    }

    /// The basis of a list that cannot rot: two variables the builder
    /// has never heard of are still removed. Replacing the sweep with a
    /// fixed override list turns this red — and that list is the shape
    /// of the next leak.
    #[test]
    fn the_builder_sweeps_variables_it_does_not_know() {
        let sandbox = Sandbox::new();
        let envs = envs(&spawn(&sandbox).build_from(hostile_host()));
        for key in ["BOOKRACK_BACKUP_DIR", "BOOKRACK_RERANKER_URL"] {
            assert_eq!(
                envs.get(key),
                Some(&None),
                "{key} reached the child instead of being swept",
            );
        }
    }

    /// The passthrough list is exactly the two PDFium names: they are
    /// not swept, so a child still finds the library CI exported.
    #[test]
    fn the_passthrough_allowlist_is_two_names() {
        let sandbox = Sandbox::new();
        let envs = envs(&spawn(&sandbox).build_from(hostile_host()));
        assert_eq!(PASSTHROUGH_ENV.len(), 2);
        for key in PASSTHROUGH_ENV {
            assert!(
                !envs.contains_key(key),
                "{key} was overridden or swept instead of inherited",
            );
        }
    }

    /// The invariant stated as an invariant rather than as a list, so
    /// it holds for variables that do not exist yet: whatever the child
    /// carries with a `BOOKRACK_` name points inside the sandbox.
    #[test]
    fn the_isolated_default_carries_no_bookrack_value_outside_the_sandbox() {
        let sandbox = Sandbox::new();
        let cmd = spawn(&sandbox).build_from(hostile_host());
        let root = sandbox.path().to_string_lossy().into_owned();
        for (key, value) in envs(&cmd) {
            let Some(value) = value else { continue };
            if !key.starts_with(BOOKRACK_PREFIX) {
                continue;
            }
            assert!(
                value.starts_with(&root),
                "{key} = {value} escapes the sandbox at {root}",
            );
        }
    }

    /// The child runs where `dotenvy` finds nothing: inside the
    /// sandbox, and with no `.env` on any path from there to the root.
    #[test]
    fn the_child_cwd_has_no_reachable_dotenv() {
        let sandbox = Sandbox::new();
        let cmd = spawn(&sandbox).build();
        let cwd = cmd.get_current_dir().expect("child runs in the sandbox");
        assert!(
            cwd.starts_with(sandbox.path()),
            "child cwd {} escapes the sandbox",
            cwd.display(),
        );
        for dir in cwd.ancestors() {
            assert!(
                !dir.join(".env").exists(),
                "a .env at {} is reachable from the child's cwd",
                dir.display(),
            );
        }
    }

    /// The named weakenings are what they say: no data root, no
    /// registry.
    #[test]
    fn the_named_deviations_unset_exactly_what_they_name() {
        let sandbox = Sandbox::new();
        let envs = envs(
            &spawn(&sandbox)
                .without_data_dir()
                .without_registry()
                .build_from(hostile_host()),
        );
        assert_eq!(
            envs.get(DATA_DIR_ENV),
            Some(&None),
            "the host data root survived without_data_dir",
        );
        assert_eq!(
            envs.get(REGISTRY_ENV),
            Some(&None),
            "the host registry survived without_registry",
        );
    }

    /// An extra variable survives the sweep even though its name
    /// carries the swept prefix.
    #[test]
    fn an_extra_variable_is_exempt_from_the_sweep() {
        let sandbox = Sandbox::new();
        let envs = envs(
            &spawn(&sandbox)
                .extra_env("BOOKRACK_CONFIRM_TIMEOUT_SECS", "3")
                .build_from(hostile_host()),
        );
        assert_eq!(
            envs.get("BOOKRACK_CONFIRM_TIMEOUT_SECS"),
            Some(&Some("3".to_string())),
        );
    }
}
