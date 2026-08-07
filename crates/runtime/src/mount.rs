// SPDX-License-Identifier: Apache-2.0

//! Runtime mounting: add a registered library to the set a running
//! daemon serves, and take one back out.
//!
//! Bring-up decides its mount set once, in [`crate::daemon`], and this
//! module runs the same sequence one library at a time afterwards:
//! resolve the library's configuration through the registry, refuse a
//! root another mount already claims, refuse a set that would disagree
//! on the reranker stage, probe the embed backend, take the root's
//! exclusive lock, open the handle, and put it in the registry.
//!
//! The root lock travels as the handle's
//! [`bookrack_ops::registry::MountGuard`], so unmounting hands the lock
//! to the last caller still holding the handle rather than dropping it
//! the moment the name leaves the registry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use bookrack_config::{Config, LibrarySelection};
use bookrack_core::Problem;
use bookrack_embed::OllamaEmbedClient;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry, MountGuard, RegistryError};
use bookrack_ops::{Caller, RerankStage};
use bookrack_session::{RootLock, is_root_lock_conflict, root_lock_path};

use crate::backend_probe::PreflightRefusal;

/// Why a runtime mount or unmount was refused.
///
/// The variants split by what the caller can do about them, which is
/// also how [`crate::control::error_map`] picks a wire code: a name the
/// registry does not carry and a name already mounted are caller input,
/// a failed consistency check is a refusal with its own three-part
/// diagnostic, and anything else is a fault the caller cannot repair.
#[derive(Debug, thiserror::Error)]
pub enum MountRefusal {
    /// The name is not in the on-disk registry, or its entry cannot be
    /// resolved to a data root.
    #[error(transparent)]
    Unresolvable(#[from] bookrack_config::ConfigError),

    /// The registry refused the operation — already mounted, or not
    /// mounted at all.
    #[error(transparent)]
    Registry(#[from] RegistryError),

    /// A consistency check refused the mount: a root another mounted
    /// library already claims, a reranker stage the set disagrees on,
    /// or an embed backend that cannot serve.
    #[error("library '{library}': {}", .problem.summary)]
    Refused {
        /// The library the check refused.
        library: String,
        /// The three-part diagnostic for the refusal.
        problem: Problem,
    },

    /// The library's data root is held by another process.
    #[error("library '{library}': {}", .problem.summary)]
    RootLocked {
        /// The library whose root could not be taken.
        library: String,
        /// The three-part diagnostic for the conflict.
        problem: Problem,
    },

    /// Opening the library's stores failed.
    #[error(transparent)]
    BringUp(#[from] eyre::Report),
}

impl From<PreflightRefusal> for MountRefusal {
    fn from(e: PreflightRefusal) -> Self {
        MountRefusal::Refused {
            library: e.library,
            problem: e.problem,
        }
    }
}

/// What an unmount has to know about the daemon around it.
///
/// Read by the handler from its own context and passed in, rather than
/// reached for here: the mounter owns the registry, and the two facts
/// below belong to the bring-up snapshot and the queue, which it does
/// not.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnmountFacts<'a> {
    /// The library this daemon came up under, `None` for a root
    /// selected by path — which has no registry name to be.
    pub primary: Option<&'a str>,
    /// How many jobs against the library are pending or running.
    pub queued_jobs: usize,
}

/// Drop-only owner of a host's whole mounted set.
///
/// With each root's lock riding on its library's handle, the registry
/// has as many `Arc` holders as there are tasks carrying a
/// [`crate::control::methods::MethodContext`] — so no one of them
/// dropping releases anything, and a host that dies without an orderly
/// shutdown would hold its roots until the last spawned task noticed.
/// This guard restores the single owner: dropping it takes every
/// library out of the registry, and each root is released as soon as
/// the last caller still using that library is done.
pub struct MountedSet(Arc<LibraryRegistry<OllamaEmbedClient>>);

impl MountedSet {
    /// Bind the guard to the registry whose mounts it releases.
    pub fn new(registry: Arc<LibraryRegistry<OllamaEmbedClient>>) -> MountedSet {
        MountedSet(registry)
    }
}

impl Drop for MountedSet {
    fn drop(&mut self) {
        let names = match self.0.list() {
            Ok(rows) => rows,
            // A poisoned map cannot be walked; the roots are released
            // when the process exits, which a poisoned lock implies is
            // about to happen anyway.
            Err(_) => return,
        };
        for row in names {
            let _ = self.0.unmount(&row.name);
        }
    }
}

/// The daemon-side capability to change the mounted set.
///
/// Holds what a mount needs and a [`crate::control::methods::MethodContext`]
/// does not carry on its own: the reranker stage handles are cloned
/// from, the [`Caller`] attribution baked into a new handle's writes,
/// and the registry itself.
pub struct Mounter {
    registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
    rerank_stage: Option<RerankStage>,
    caller: Caller,
}

impl Mounter {
    /// Bind a mounter to the registry it maintains.
    pub fn new(
        registry: Arc<LibraryRegistry<OllamaEmbedClient>>,
        rerank_stage: Option<RerankStage>,
        caller: Caller,
    ) -> Mounter {
        Mounter {
            registry,
            rerank_stage,
            caller,
        }
    }

    /// Whether `name` is in the mounted set right now.
    pub fn is_mounted(&self, name: &str) -> bool {
        self.registry.get(Some(name)).is_ok()
    }

    /// Open a registered library and add it to the served set.
    ///
    /// Runs bring-up's checks in bring-up's order, so a library that
    /// starts cleanly also mounts cleanly and the two paths refuse the
    /// same inputs with the same wording. `name` must already be in the
    /// on-disk registry: resolution goes through
    /// [`Config::resolve`]'s named-selection branch, which outranks
    /// `BOOKRACK_DATA_DIR`, so the root is the one the registry
    /// declares even on a machine that sets the variable.
    pub async fn mount(&self, name: &str) -> Result<(), MountRefusal> {
        if self.is_mounted(name) {
            return Err(RegistryError::AlreadyMounted {
                name: name.to_string(),
            }
            .into());
        }

        let cfg = Arc::new(Config::resolve(&LibrarySelection {
            data_dir: None,
            library: Some(name.to_string()),
        })?);

        self.claim_root(name, &cfg)?;
        let mounts = [(name.to_string(), Arc::clone(&cfg))];
        // The already-mounted set agreed on a reranker stage at
        // bring-up; the new library has to agree with it too, and a
        // disagreement refuses the mount rather than restarting a
        // backend the serving libraries did not ask to have restarted.
        self.agree_on_reranker(name, &cfg)?;
        crate::backend_probe::preflight_embed_backends(&mounts).await?;

        let guard = self.lock_root(name, &cfg)?;
        let handle = crate::daemon::build_library_handle(
            cfg,
            name,
            self.rerank_stage.as_ref(),
            self.caller.clone(),
            guard,
        )
        .await?;
        self.registry.mount(handle)?;
        Ok(())
    }

    /// Take a library out of the served set and hand its handle back.
    ///
    /// Dropping the returned handle asks for the root lock to be
    /// released; the release itself happens when the last caller still
    /// holding that handle is done with it.
    ///
    /// Three libraries are refused, each because unmounting it would
    /// leave the daemon describing something it no longer serves:
    ///
    /// * the **registry default** — the no-name route would resolve to
    ///   a key the mounted set no longer has, taking every unnamed call
    ///   down with it;
    /// * the **primary** — the daemon's identity is a bring-up fact, so
    ///   the `status` card would go on naming a library that left;
    /// * a library with **queued work** — a pending or running job
    ///   against it would burn on the next pull, and the operator would
    ///   see a failed ingest rather than a refused unmount.
    ///
    /// "The last library" needs no rule of its own: the registry's
    /// default is always inside the mounted set, so a one-library
    /// daemon's only library is its default and the first rule catches
    /// it.
    pub fn unmount(
        &self,
        name: &str,
        facts: UnmountFacts<'_>,
    ) -> Result<Arc<LibraryHandle<OllamaEmbedClient>>, MountRefusal> {
        // Resolve first: a name the daemon does not serve gets the
        // "unknown library" refusal rather than one of the three below,
        // which would describe a library that is not there.
        self.registry
            .get(Some(name))
            .map_err(MountRefusal::Registry)?;

        if self
            .registry
            .default_name()
            .map_err(MountRefusal::Registry)?
            == name
        {
            return Err(MountRefusal::Refused {
                library: name.to_string(),
                problem: Problem::new("cannot unmount the registry's default library")
                    .detail(
                        "A call that names no library resolves to the default, so unmounting \
                         it would leave every unnamed call with nothing to route to."
                            .to_string(),
                    )
                    .hint(format!(
                        "Move the pointer to another served library first — \
                         `bookrack libraries default <other>` — then `bookrack libraries \
                         unmount {name}`.",
                    )),
            });
        }

        if facts.primary == Some(name) {
            return Err(MountRefusal::Refused {
                library: name.to_string(),
                problem: Problem::new("cannot unmount the library the daemon came up under")
                    .detail(
                        "The daemon's identity — what `bookrack status` reports as its \
                         library and data root — is fixed at bring-up, so unmounting this \
                         one would leave the status card describing a library that is no \
                         longer served."
                            .to_string(),
                    )
                    .hint(
                        "Stop the daemon with `bookrack quit` and start it under another \
                         library; changing which library a daemon is identified by means \
                         changing the daemon."
                            .to_string(),
                    ),
            });
        }

        // Pending and running both, not running alone: a pending job
        // left behind starts after the handle is gone and fails, which
        // is the silent loss this refusal exists to prevent.
        let queued = facts.queued_jobs;
        if queued > 0 {
            return Err(MountRefusal::Refused {
                library: name.to_string(),
                problem: Problem::new("cannot unmount a library with queued work")
                    .detail(format!(
                        "{queued} job(s) against this library are pending or running; \
                         each would fail on its next pull once the library stops being \
                         served.",
                    ))
                    .hint(
                        "Wait for the queue to drain, or drop the pending jobs with \
                         `bookrack queue clear`, then unmount."
                            .to_string(),
                    ),
            });
        }

        Ok(self.registry.unmount(name)?)
    }

    /// Refuse a root another mounted library already serves, with the
    /// judgement bring-up makes at startup: two names on one root is a
    /// registry to fix, not a state to serve.
    fn claim_root(&self, name: &str, cfg: &Config) -> Result<(), MountRefusal> {
        let root = canonical(cfg.data_dir());
        let mounted = self
            .registry
            .list()
            .map_err(MountRefusal::Registry)?
            .into_iter()
            .map(|row| (canonical(&row.data_dir), row.name))
            .collect::<HashMap<PathBuf, String>>();
        if let Some(previous) = mounted.get(&root) {
            return Err(MountRefusal::Refused {
                library: name.to_string(),
                problem: Problem::new("cannot mount two libraries on one data root")
                    .detail(format!(
                        "Library {previous:?} is already served from {}.",
                        cfg.data_dir().display(),
                    ))
                    .hint(
                        "Registry entries have to name distinct roots. Run \
                         `bookrack libraries list` to see what each entry points at, \
                         then repoint or remove one of them.",
                    ),
            });
        }
        Ok(())
    }

    /// Refuse a library whose index profile resolves to a different
    /// reranker stage than the mounted set already agreed on. One
    /// backend serves every mounted library, so the alternative is
    /// degrading libraries that are already being served.
    fn agree_on_reranker(&self, name: &str, cfg: &Arc<Config>) -> Result<(), MountRefusal> {
        let mut mounts: Vec<(String, Arc<Config>)> = self
            .registry
            .list()
            .map_err(MountRefusal::Registry)?
            .into_iter()
            .filter_map(|row| {
                let handle = self.registry.get(Some(&row.name)).ok()?;
                Some((row.name, handle.cfg_arc()))
            })
            .collect();
        mounts.push((name.to_string(), Arc::clone(cfg)));
        crate::rerank_supervisor::agreed_reranker_config(&mounts)?;
        Ok(())
    }

    /// Take the data root's exclusive lock, or explain who holds it.
    ///
    /// A root that cannot host a lock file at all is served unlocked,
    /// exactly as bring-up serves it: read-only media have no writers
    /// to exclude. Such a library mounts with no guard, and unmounting
    /// it has no lock to release.
    fn lock_root(&self, name: &str, cfg: &Config) -> Result<Option<MountGuard>, MountRefusal> {
        match RootLock::acquire(cfg.data_dir(), std::process::id(), "daemon") {
            Ok(lock) => {
                tracing::info!(
                    library = %name,
                    path = %root_lock_path(cfg.data_dir()).display(),
                    "bookrack data root lock acquired",
                );
                Ok(Some(Arc::new(lock) as MountGuard))
            }
            Err(err) if is_root_lock_conflict(&err) => Err(MountRefusal::RootLocked {
                library: name.to_string(),
                problem: Problem::new("cannot lock the data root of the library being mounted")
                    .detail(format!("{err:#}"))
                    .hint(
                        "Another process holds the root. Stop whatever is using it — \
                         a second daemon, or an offline destructive command — then mount again.",
                    ),
            }),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    library = %name,
                    root = %cfg.data_dir().display(),
                    "data root lock could not be created; serving the root unlocked",
                );
                Ok(None)
            }
        }
    }
}

/// A path compared by identity where the filesystem can answer, by
/// spelling where it cannot — the same fallback bring-up's duplicate
/// check uses.
fn canonical(root: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}
