// SPDX-License-Identifier: Apache-2.0

//! A throwaway host: the directory tree an isolated bookrack process
//! reads and writes instead of the operator's own.

use std::path::{Path, PathBuf};

use bookrack_config::DEFAULT_REGISTRY_NAME;
use tempfile::TempDir;

/// One tempdir tree standing in for every host location bookrack
/// consults: a home directory with its XDG children, a data root, a
/// runtime directory, a daemon state directory, a working directory for
/// spawned children, and a `roots/` parent for additional data roots.
///
/// Construction materialises an empty registry file at
/// [`Sandbox::registry_path`]. [`bookrack_config::load_registry`] maps
/// every read failure of the file named by `BOOKRACK_REGISTRY` —
/// including `NotFound` — onto a hard error, so a sandbox that named a
/// registry without creating it would fail every resolution that
/// reaches it.
///
/// The tree is removed when the `Sandbox` drops.
pub struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    /// Build the tree and write the empty registry.
    ///
    /// Panics on any I/O failure: a sandbox that is only partly built
    /// cannot isolate anything, and a test that proceeds against one
    /// reports on the host instead.
    pub fn new() -> Sandbox {
        let root = tempfile::tempdir().expect("sandbox tempdir");
        let sandbox = Sandbox { root };
        for dir in [
            sandbox.home(),
            sandbox.config_home(),
            sandbox.data_home(),
            sandbox.cache_home(),
            sandbox.data_dir(),
            sandbox.runtime_dir(),
            sandbox.daemon_state_dir(),
            sandbox.cwd(),
            sandbox.roots(),
        ] {
            std::fs::create_dir_all(&dir).expect("sandbox subdirectory");
        }
        std::fs::write(sandbox.registry_path(), "").expect("sandbox registry file");
        sandbox
    }

    /// Root of the whole tree.
    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Home directory the child process sees as `HOME`.
    pub fn home(&self) -> PathBuf {
        self.path().join("home")
    }

    /// Directory the child sees as `XDG_CONFIG_HOME`.
    pub fn config_home(&self) -> PathBuf {
        self.home().join(".config")
    }

    /// Directory the child sees as `XDG_DATA_HOME`.
    pub fn data_home(&self) -> PathBuf {
        self.home().join(".local").join("share")
    }

    /// Directory the child sees as `XDG_CACHE_HOME`.
    pub fn cache_home(&self) -> PathBuf {
        self.home().join(".cache")
    }

    /// Data root named by `BOOKRACK_DATA_DIR`.
    pub fn data_dir(&self) -> PathBuf {
        self.path().join("data")
    }

    /// Session runtime directory named by `BOOKRACK_RUNTIME_DIR`.
    pub fn runtime_dir(&self) -> PathBuf {
        self.path().join("runtime")
    }

    /// Daemon state directory named by `BOOKRACK_DAEMON_STATE_DIR`.
    pub fn daemon_state_dir(&self) -> PathBuf {
        self.path().join("daemon-state")
    }

    /// Registry file named by `BOOKRACK_REGISTRY`. Exists and is empty
    /// after [`Sandbox::new`].
    pub fn registry_path(&self) -> PathBuf {
        self.path().join(DEFAULT_REGISTRY_NAME)
    }

    /// Working directory spawned children run in. Holds no `.env`, and
    /// no ancestor of it inside the sandbox does either, so `dotenvy`'s
    /// upward search from a child cannot reach the repository's.
    pub fn cwd(&self) -> PathBuf {
        self.path().join("cwd")
    }

    /// Session lock file inside [`Sandbox::runtime_dir`].
    pub fn tty_lock_path(&self) -> PathBuf {
        self.runtime_dir().join(bookrack_session::tty_lock_name())
    }

    /// Parent of the named data roots.
    fn roots(&self) -> PathBuf {
        self.path().join("roots")
    }

    /// An additional data root under `roots/`, created on demand. Use
    /// for the second and later libraries a registry names.
    pub fn data_root(&self, name: &str) -> PathBuf {
        let path = self.roots().join(name);
        std::fs::create_dir_all(&path).expect("sandbox data root");
        path
    }

    /// Overwrite the registry with `toml` and return its path.
    pub fn write_registry(&self, toml: &str) -> PathBuf {
        let path = self.registry_path();
        std::fs::write(&path, toml).expect("write sandbox registry");
        path
    }

    /// Overwrite the registry with `entries` in the table form, plus an
    /// optional `default`, and return its path.
    pub fn write_registry_entries(
        &self,
        default: Option<&str>,
        entries: &[(&str, &Path)],
    ) -> PathBuf {
        let mut toml = String::new();
        if let Some(name) = default {
            toml.push_str(&format!("default = {}\n\n", toml_string(name)));
        }
        for (name, root) in entries {
            toml.push_str(&format!("[libraries.{name}]\n"));
            toml.push_str(&format!(
                "data_dir = {}\n\n",
                toml_string(&root.display().to_string()),
            ));
        }
        self.write_registry(&toml)
    }
}

impl Default for Sandbox {
    fn default() -> Sandbox {
        Sandbox::new()
    }
}

/// Quote `value` as a TOML basic string. JSON string escaping is a
/// subset of TOML's, so the JSON encoder produces a valid basic string
/// for every path a tempdir can hand back.
fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("string encodes as JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `load_registry` treats a `NotFound` on the file named by
    /// `BOOKRACK_REGISTRY` as a hard error, so the file has to exist
    /// before any migrated test resolves a config. Dropping the write
    /// in [`Sandbox::new`] turns every such test into
    /// `RegistryUnreadable`.
    #[test]
    fn an_empty_registry_file_exists_after_construction() {
        let sandbox = Sandbox::new();
        let path = sandbox.registry_path();
        assert!(path.is_file(), "{} was not created", path.display());
        assert_eq!(std::fs::read_to_string(&path).expect("read registry"), "");
    }

    /// The whole tree exists, so nothing under it is created by
    /// accident at the host's expense.
    #[test]
    fn every_named_location_exists_and_lives_under_one_root() {
        let sandbox = Sandbox::new();
        for dir in [
            sandbox.home(),
            sandbox.config_home(),
            sandbox.data_home(),
            sandbox.cache_home(),
            sandbox.data_dir(),
            sandbox.runtime_dir(),
            sandbox.daemon_state_dir(),
            sandbox.cwd(),
            sandbox.data_root("alpha"),
        ] {
            assert!(dir.is_dir(), "{} is not a directory", dir.display());
            assert!(dir.starts_with(sandbox.path()));
        }
        assert_eq!(
            sandbox.tty_lock_path(),
            sandbox.runtime_dir().join("bookrack.tty.lock"),
        );
    }

    /// The registry writer emits what the parser reads back, including
    /// the `default` key — the two multi-library suites relied on two
    /// verbatim copies of this TOML before.
    #[test]
    fn written_entries_parse_back_as_the_registry_the_caller_described() {
        let sandbox = Sandbox::new();
        let alpha = sandbox.data_root("alpha");
        let beta = sandbox.data_root("beta");
        let path = sandbox.write_registry_entries(
            Some("alpha"),
            &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
        );
        let entries = bookrack_config::list_libraries_at(&path).expect("registry parses");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert_eq!(entries[0].data_dir, alpha);
        assert_eq!(entries[1].data_dir, beta);
        assert!(entries[0].is_default, "default = \"alpha\" was not written");
        assert!(!entries[1].is_default);
    }

    /// A registry written without a default has none, so the tests
    /// that assert an unconfigured resolution keep failing for the
    /// reason they name.
    #[test]
    fn written_entries_without_a_default_carry_none() {
        let sandbox = Sandbox::new();
        let alpha = sandbox.data_root("alpha");
        let path = sandbox.write_registry_entries(None, &[("alpha", alpha.as_path())]);
        let entries = bookrack_config::list_libraries_at(&path).expect("registry parses");
        assert!(entries.iter().all(|e| !e.is_default));
    }
}
