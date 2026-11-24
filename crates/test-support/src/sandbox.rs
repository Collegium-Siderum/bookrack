// SPDX-License-Identifier: Apache-2.0

//! A stand-in host: the directory tree an isolated bookrack process
//! reads and writes instead of the operator's own.

use std::io;
use std::path::{Path, PathBuf};

use bookrack_config::{
    DAEMON_STATE_DIR_ENV, DEFAULT_REGISTRY_NAME, LibraryEntryFields, LibraryKind,
};
use bookrack_session::RUNTIME_DIR_ENV;
use tempfile::TempDir;

/// Where a sandbox's tree lives.
enum Root {
    /// A tempdir, removed when the sandbox drops.
    Temp(TempDir),
    /// A directory the caller chose, kept when the sandbox drops. Held
    /// in canonical form, so every path derived from it compares
    /// against a canonical path without another resolution.
    Kept(PathBuf),
}

/// One tree standing in for every host location bookrack consults: a
/// home directory with its XDG children, a data root, a runtime
/// directory, a daemon state directory, a working directory for spawned
/// children, a `roots/` parent for additional data roots, and an
/// `inputs/` directory for files a command is handed.
///
/// Construction materialises an empty registry file at
/// [`Sandbox::registry_path`]. [`bookrack_config::load_registry`] maps
/// every read failure of the file named by `BOOKRACK_REGISTRY` —
/// including `NotFound` — onto a hard error, so a sandbox that named a
/// registry without creating it would fail every resolution that
/// reaches it.
///
/// [`Sandbox::new`] builds the tree in a tempdir that is removed when
/// the `Sandbox` drops; [`Sandbox::create_at`] builds it where the
/// caller says and leaves it there.
pub struct Sandbox {
    root: Root,
}

impl Sandbox {
    /// Build the tree in a fresh tempdir and write the empty registry.
    ///
    /// Panics on any I/O failure: a sandbox that is only partly built
    /// cannot isolate anything, and a test that proceeds against one
    /// reports on the host instead.
    pub fn new() -> Sandbox {
        let root = tempfile::tempdir().expect("sandbox tempdir");
        let sandbox = Sandbox {
            root: Root::Temp(root),
        };
        sandbox.build_tree().expect("sandbox tree");
        sandbox
    }

    /// Build the tree at `path`, which must not exist or must be an
    /// empty directory, and keep it when the sandbox drops. The root is
    /// held in canonical form.
    pub fn create_at(path: &Path) -> io::Result<Sandbox> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => {
                if std::fs::read_dir(path)?.next().is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not empty", path.display()),
                    ));
                }
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a directory", path.display()),
                ));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        std::fs::create_dir_all(path)?;
        let sandbox = Sandbox {
            root: Root::Kept(path.canonicalize()?),
        };
        sandbox.build_tree()?;
        Ok(sandbox)
    }

    /// Open the tree a [`Sandbox::create_at`] built earlier, checking
    /// that every named location is a real directory and the registry a
    /// real file. Writes nothing.
    pub fn open(path: &Path) -> io::Result<Sandbox> {
        let sandbox = Sandbox {
            root: Root::Kept(path.canonicalize()?),
        };
        for dir in sandbox.directories() {
            let meta = std::fs::symlink_metadata(&dir)?;
            if !meta.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a directory", dir.display()),
                ));
            }
        }
        let registry = sandbox.registry_path();
        if !std::fs::symlink_metadata(&registry)?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a regular file", registry.display()),
            ));
        }
        Ok(sandbox)
    }

    /// Create every directory and the empty registry.
    fn build_tree(&self) -> io::Result<()> {
        for dir in self.directories() {
            std::fs::create_dir_all(&dir)?;
        }
        std::fs::write(self.registry_path(), "")
    }

    /// Every directory the tree holds, in creation order.
    fn directories(&self) -> Vec<PathBuf> {
        vec![
            self.home(),
            self.config_home(),
            self.data_home(),
            self.cache_home(),
            self.xdg_runtime_dir(),
            self.data_dir(),
            self.runtime_dir(),
            self.daemon_state_dir(),
            self.cwd(),
            self.roots(),
            self.inputs(),
        ]
    }

    /// Root of the whole tree.
    pub fn path(&self) -> &Path {
        match &self.root {
            Root::Temp(dir) => dir.path(),
            Root::Kept(path) => path,
        }
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

    /// The per-user data directory a child resolves in this sandbox:
    /// the parent of every managed native component's directory. On
    /// macOS the platform rule ignores `XDG_DATA_HOME` and derives it
    /// from the home directory.
    pub fn managed_data_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home().join("Library").join("Application Support")
        } else {
            self.data_home()
        }
    }

    /// Link the PDFium builds installed on the host into this sandbox's
    /// managed directory, so a process running in it finds the library
    /// through its ordinary search. For a test that extracts a PDF; a
    /// sandbox that does not call this holds no library, whatever the
    /// host has. A host with none installed leaves the sandbox as it
    /// was, and the test's own PDFium gate decides what that means.
    pub fn with_host_pdfium(self) -> Sandbox {
        match crate::host_slot::host_pdfium_root() {
            Some(root) => self.with_pdfium_from(root),
            None => self,
        }
    }

    /// [`Sandbox::with_host_pdfium`] with the host's root passed in.
    pub(crate) fn with_pdfium_from(self, host_root: &Path) -> Sandbox {
        let target = self.managed_data_dir().join("bookrack").join("pdfium");
        crate::host_slot::link_tree(host_root, &target).expect("link the host's PDFium");
        self
    }

    /// Directory the child sees as `XDG_CACHE_HOME`.
    pub fn cache_home(&self) -> PathBuf {
        self.home().join(".cache")
    }

    /// Directory the child sees as `XDG_RUNTIME_DIR`. On Linux the
    /// platform runtime directory derives from it, and that candidate
    /// shows up in the configuration report whether or not it wins.
    pub fn xdg_runtime_dir(&self) -> PathBuf {
        self.path().join("xdg-runtime")
    }

    /// The sandbox's own data root. Nothing selects it until
    /// [`Sandbox::name_data_dir`] registers it under a name.
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
    /// after construction.
    pub fn registry_path(&self) -> PathBuf {
        self.path().join(DEFAULT_REGISTRY_NAME)
    }

    /// Working directory spawned children run in. Holds no `.env`, and
    /// no ancestor of it inside the sandbox does either, so `dotenvy`'s
    /// upward search from a child cannot reach the repository's.
    pub fn cwd(&self) -> PathBuf {
        self.path().join("cwd")
    }

    /// Directory for files a command under test is handed as input.
    pub fn inputs(&self) -> PathBuf {
        self.path().join("inputs")
    }

    /// Session lock file inside [`Sandbox::runtime_dir`].
    pub fn tty_lock_path(&self) -> PathBuf {
        self.runtime_dir().join(bookrack_session::tty_lock_name())
    }

    /// The one list of host-visible location variables and where each
    /// points inside this tree. Both the child builder and the process
    /// environment apply exactly this list, so there is no second copy
    /// to drift from it.
    pub fn host_vars(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("HOME", self.home()),
            ("XDG_CONFIG_HOME", self.config_home()),
            ("XDG_DATA_HOME", self.data_home()),
            ("XDG_CACHE_HOME", self.cache_home()),
            ("XDG_RUNTIME_DIR", self.xdg_runtime_dir()),
            (RUNTIME_DIR_ENV, self.runtime_dir()),
            (DAEMON_STATE_DIR_ENV, self.daemon_state_dir()),
        ]
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

    /// Overwrite the registry with `entries`, plus an optional
    /// `default`, and return its path.
    ///
    /// Each entry is registered the way [`Sandbox::name_data_dir`]
    /// registers the data root, so a root that exists carries a
    /// manifest and its entry the manifest's identity. `default` must name one of `entries`; a
    /// registry whose default points nowhere is written with
    /// [`Sandbox::write_registry`].
    pub fn write_registry_entries(
        &self,
        default: Option<&str>,
        entries: &[(&str, &Path)],
    ) -> PathBuf {
        let path = self.write_registry("");
        for (name, root) in entries {
            self.register_root(name, root);
        }
        if let Some(name) = default {
            bookrack_config::set_default_library(&path, name).unwrap_or_else(|err| {
                panic!("point the sandbox registry's default at {name}: {err}")
            });
        }
        path
    }

    /// Register [`Sandbox::data_dir`] under `name` in the sandbox
    /// registry, beside any entry already there, and return the root.
    /// A child then reaches it with [`crate::Spawn::library`].
    ///
    /// The root gets a manifest named `name`, of kind `prod`, unless it
    /// already holds one, and the entry caches the manifest's identity,
    /// as an operator's registration does.
    pub fn name_data_dir(&self, name: &str) -> PathBuf {
        let root = self.data_dir();
        self.register_root(name, &root);
        root
    }

    /// Add or replace the entry `name` → `root` in the sandbox
    /// registry, leaving every other entry as it is.
    ///
    /// A root that exists and holds no manifest gets a fresh one named
    /// `name`, of kind `prod`; a root that already holds one keeps it.
    /// The entry then caches that manifest's identity, as an operator's
    /// registration does. A root that does not exist is recorded by
    /// path alone and is not created, and neither is an entry identity
    /// read off a manifest that does not parse.
    fn register_root(&self, name: &str, root: &Path) {
        let manifest = if root.is_dir() {
            match bookrack_config::load_manifest(root) {
                Ok(Some(manifest)) => Some(manifest),
                Ok(None) => {
                    let manifest = bookrack_config::new_manifest(name, LibraryKind::Prod, None);
                    bookrack_config::write_manifest(root, &manifest).unwrap_or_else(|err| {
                        panic!("write a manifest under {}: {err}", root.display())
                    });
                    Some(manifest)
                }
                Err(_) => None,
            }
        } else {
            None
        };
        let fields = match manifest {
            Some(manifest) => LibraryEntryFields {
                data_dir: root.to_path_buf(),
                kind: manifest.kind,
                description: manifest.description,
                index_profile: manifest.index_profile,
                created_at: manifest.created_at,
                uuid: Some(manifest.uuid),
            },
            None => LibraryEntryFields {
                data_dir: root.to_path_buf(),
                kind: LibraryKind::Prod,
                description: None,
                index_profile: None,
                created_at: None,
                uuid: None,
            },
        };
        bookrack_config::upsert_library_entry(&self.registry_path(), name, &fields)
            .unwrap_or_else(|err| panic!("register {name} in the sandbox registry: {err}"));
    }
}

impl Default for Sandbox {
    fn default() -> Sandbox {
        Sandbox::new()
    }
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
            sandbox.xdg_runtime_dir(),
            sandbox.data_dir(),
            sandbox.runtime_dir(),
            sandbox.daemon_state_dir(),
            sandbox.cwd(),
            sandbox.inputs(),
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

    /// Every root an entry names gets a manifest, and the entry caches
    /// its identity: the uuid a mount compares against the root's is
    /// the one under the root, and the kind is the manifest's.
    #[test]
    fn a_written_entry_carries_the_identity_of_its_roots_manifest() {
        let sandbox = Sandbox::new();
        let alpha = sandbox.data_root("alpha");
        let path = sandbox.write_registry_entries(None, &[("alpha", alpha.as_path())]);
        let manifest = bookrack_config::load_manifest(&alpha)
            .expect("manifest parses")
            .expect("the root carries a manifest");
        assert_eq!(manifest.name, "alpha");
        assert_eq!(manifest.kind, LibraryKind::Prod);
        let entries = bookrack_config::list_libraries_at(&path).expect("registry parses");
        assert_eq!(entries[0].uuid.as_deref(), Some(manifest.uuid.as_str()));
        assert_eq!(entries[0].kind, manifest.kind);
    }

    /// A root that already holds a manifest keeps it, byte for byte,
    /// and the entry takes that manifest's identity rather than a new
    /// one.
    #[test]
    fn an_existing_manifest_is_kept_and_cached() {
        let sandbox = Sandbox::new();
        let alpha = sandbox.data_root("alpha");
        let own = bookrack_config::new_manifest("born-elsewhere", LibraryKind::Test, None);
        bookrack_config::write_manifest(&alpha, &own).expect("write manifest");
        let before =
            std::fs::read(alpha.join(bookrack_config::MANIFEST_FILENAME)).expect("read manifest");
        let path = sandbox.write_registry_entries(None, &[("alpha", alpha.as_path())]);
        let after =
            std::fs::read(alpha.join(bookrack_config::MANIFEST_FILENAME)).expect("read manifest");
        assert_eq!(before, after, "an existing manifest was rewritten");
        let entries = bookrack_config::list_libraries_at(&path).expect("registry parses");
        assert_eq!(entries[0].uuid.as_deref(), Some(own.uuid.as_str()));
        assert_eq!(entries[0].kind, LibraryKind::Test);
    }

    /// An entry whose root does not exist is a missing-root fixture:
    /// the root is not created, and the entry carries no identity a
    /// mount could compare.
    #[test]
    fn a_missing_root_is_recorded_by_path_alone() {
        let sandbox = Sandbox::new();
        let gone = sandbox.path().join("roots").join("gone");
        let path = sandbox.write_registry_entries(None, &[("gone", gone.as_path())]);
        assert!(!gone.exists(), "a missing root was created");
        let entries = bookrack_config::list_libraries_at(&path).expect("registry parses");
        assert_eq!(entries[0].data_dir, gone);
        assert_eq!(entries[0].uuid, None);
    }

    /// Naming the data root adds an entry beside the ones already
    /// written and leaves them as they were; the named root carries a
    /// manifest whose uuid the entry caches.
    #[test]
    fn naming_the_data_root_adds_an_entry_beside_the_others() {
        let sandbox = Sandbox::new();
        let alpha = sandbox.data_root("alpha");
        sandbox.write_registry_entries(None, &[("alpha", alpha.as_path())]);
        let root = sandbox.name_data_dir("main");
        assert_eq!(root, sandbox.data_dir());
        let entries =
            bookrack_config::list_libraries_at(&sandbox.registry_path()).expect("registry parses");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "main"]);
        let main = entries.iter().find(|e| e.name == "main").expect("main");
        assert_eq!(main.data_dir, sandbox.data_dir());
        let manifest = bookrack_config::load_manifest(&sandbox.data_dir())
            .expect("manifest parses")
            .expect("the data root carries a manifest");
        assert_eq!(main.uuid.as_deref(), Some(manifest.uuid.as_str()));
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

    /// A kept tree outlives its `Sandbox`, is held canonically, opens
    /// again, and is refused where something already is.
    #[test]
    fn a_kept_tree_survives_its_sandbox_and_opens_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wanted = dir.path().join("host");
        let root = {
            let sandbox = Sandbox::create_at(&wanted).expect("create_at");
            assert_eq!(
                sandbox.path(),
                wanted.canonicalize().expect("canonical").as_path()
            );
            sandbox.path().to_path_buf()
        };
        assert!(
            root.join("cwd").is_dir(),
            "the kept tree was removed on drop"
        );
        let reopened = Sandbox::open(&wanted).expect("open");
        assert_eq!(reopened.path(), root.as_path());
        assert!(
            Sandbox::create_at(&wanted).is_err(),
            "a non-empty directory must be refused"
        );
        std::fs::remove_dir(root.join("inputs")).expect("remove a directory");
        assert!(
            Sandbox::open(&wanted).is_err(),
            "an incomplete tree must not open"
        );
    }

    /// The location list names every variable the environment side
    /// isolates, each pointing into the tree.
    #[test]
    fn host_vars_name_every_location_inside_the_tree() {
        let sandbox = Sandbox::new();
        let vars = sandbox.host_vars();
        let keys: Vec<&str> = vars.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            [
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
                "XDG_RUNTIME_DIR",
                RUNTIME_DIR_ENV,
                DAEMON_STATE_DIR_ENV,
            ]
        );
        for (key, path) in vars {
            assert!(
                path.starts_with(sandbox.path()),
                "{key} points outside the tree"
            );
            assert!(path.is_dir(), "{key} = {} does not exist", path.display());
        }
    }
}
