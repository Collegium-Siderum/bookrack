// SPDX-License-Identifier: Apache-2.0

//! Turns a data root into the registry name a daemon can be asked for.
//!
//! `--data-dir` and `BOOKRACK_DATA_DIR` select a root by path. A daemon
//! mounts every registered library, so what it takes on the wire is a
//! name; a path has to be translated before it can travel, and the
//! translation is the registry's to make — by the root's manifest uuid
//! first, then by the path itself.
//!
//! Not every root has a name, and that is ordinary: a library the
//! registry never learned about is a working setup, served by a daemon
//! started under the same path. Such a root produces no selection at
//! all, and the question of whether the running daemon is the one
//! serving it is settled against the daemon — see
//! [`crate::error::BookrackCliError::RootNotRoutable`] and the caller
//! in the control-plane client, which asks. Refusing here would refuse
//! every command on a setup that has nothing wrong with it, including
//! the ones that run with no daemon at all.
//!
//! One case is refused before any of that: a root whose manifest
//! carries the identity of a registered library pointing **somewhere
//! else**. The registry knows this library and places it at another
//! path, so acting on either one silently answers for a directory the
//! caller did not name. Both paths go in the message and nothing is
//! done.
//!
//! Locally resolving commands never come here: for them a path is a
//! real switch into that root, and the registry has no say.

use std::path::{Path, PathBuf};

use bookrack_config::{DATA_DIR_ENV, LibrarySelection, RootIdentity};
use bookrack_core::{Problem, ProblemData};

use crate::error::BookrackCliError;

/// What a daemon-routed invocation carries onto the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routing {
    /// No library was selected; the daemon applies its own default.
    Unselected,
    /// A registry name, ready to send.
    Named(String),
    /// A root no registry entry claims. It cannot be named, but it may
    /// still be the very root the running daemon serves, which only the
    /// daemon can say.
    UnclaimedRoot(PathBuf),
}

/// Translate this invocation's selection into what the client sends.
pub fn routing_for(selection: &LibrarySelection) -> Result<Routing, BookrackCliError> {
    let env = std::env::var(DATA_DIR_ENV).ok();
    routing_with(selection, env.as_deref(), bookrack_config::identify_root)
}

/// [`routing_for`] against an explicit environment value and registry
/// lookup, so each outcome can be exercised without a registry on disk.
fn routing_with(
    selection: &LibrarySelection,
    env_data_dir: Option<&str>,
    identify: impl Fn(&Path) -> RootIdentity,
) -> Result<Routing, BookrackCliError> {
    if let Some(name) = &selection.library {
        return Ok(Routing::Named(name.clone()));
    }
    let Some(root) = path_selection(selection, env_data_dir) else {
        return Ok(Routing::Unselected);
    };
    match identify(&root) {
        RootIdentity::Named { name, .. } => Ok(Routing::Named(name)),
        RootIdentity::UuidElsewhere { name, entry_root } => {
            Err(BookrackCliError::RootNotRoutable {
                problem: uuid_elsewhere(&root, &name, &entry_root),
            })
        }
        RootIdentity::Unregistered => Ok(Routing::UnclaimedRoot(root)),
    }
}

/// The root a path-shaped selection expresses, in the precedence the
/// config crate documents: the flag, then the environment variable.
fn path_selection(selection: &LibrarySelection, env_data_dir: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = &selection.data_dir {
        return Some(path.clone());
    }
    env_data_dir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The refusal an unnamed root earns once the daemon has answered that
/// it serves a different one. Both roots are named: which two
/// directories are in play is the whole question.
pub fn serves_another_root(asked: &Path, served: &Path) -> Problem {
    Problem {
        summary: format!("running daemon does not serve \"{}\"", asked.display()),
        data: ProblemData {
            detail: Some(format!(
                "It serves {}, and the root you named is in no registry entry, so the \
                 call cannot be routed to it.",
                served.display()
            )),
            hint: Some(format!(
                "Register the root and name it — `bookrack libraries register <name> \
                 --data-dir {}` — or stop this daemon with `bookrack quit` and start \
                 one on that root.",
                asked.display()
            )),
            retryable: false,
        },
    }
}

fn uuid_elsewhere(root: &Path, name: &str, entry_root: &Path) -> Problem {
    Problem {
        summary: format!("cannot tell which library \"{}\" is", root.display()),
        data: ProblemData {
            detail: Some(format!(
                "Its manifest carries the identity of the registered library '{name}', \
                 which the registry places at {}. Two roots claiming one identity is a \
                 copy or a move the registry was not told about.",
                entry_root.display()
            )),
            hint: Some(format!(
                "Point the registry at this root with `bookrack libraries register \
                 {name} --data-dir {}`, or run the command against the registered \
                 one with `--library {name}`.",
                root.display()
            )),
            retryable: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> impl Fn(&Path) -> RootIdentity + '_ {
        move |_| RootIdentity::Named {
            name: name.to_string(),
            by: bookrack_config::LibraryIdentification::Path,
        }
    }

    fn unclaimed(_: &Path) -> RootIdentity {
        RootIdentity::Unregistered
    }

    /// A name travels as itself; the registry is not consulted, because
    /// the daemon is the one that resolves a name.
    #[test]
    fn a_named_selection_needs_no_translation() {
        let selection = LibrarySelection {
            data_dir: None,
            library: Some("beta".into()),
        };
        let routing = routing_with(&selection, None, |_| panic!("a name must not be looked up"))
            .expect("a name routes");
        assert_eq!(routing, Routing::Named("beta".to_string()));
    }

    /// Both path channels sugar the same way. The environment variable
    /// is the one a shell carries into every invocation, so leaving it
    /// out would be the same silence this whole module removes.
    #[test]
    fn both_path_channels_become_the_registry_name() {
        let by_flag = LibrarySelection {
            data_dir: Some(PathBuf::from("/roots/beta")),
            library: None,
        };
        assert_eq!(
            routing_with(&by_flag, None, named("beta")).expect("the flag sugars"),
            Routing::Named("beta".to_string())
        );

        let by_env = LibrarySelection::default();
        assert_eq!(
            routing_with(&by_env, Some("/roots/beta"), named("beta")).expect("the env sugars"),
            Routing::Named("beta".to_string())
        );
    }

    /// The flag outranks the environment variable, so a translation
    /// that read the wrong one would route to the wrong library.
    #[test]
    fn the_flag_outranks_the_environment() {
        let selection = LibrarySelection {
            data_dir: Some(PathBuf::from("/roots/beta")),
            library: None,
        };
        let seen = std::cell::RefCell::new(Vec::new());
        let routing = routing_with(&selection, Some("/roots/alpha"), |root| {
            seen.borrow_mut().push(root.to_path_buf());
            RootIdentity::Named {
                name: "beta".to_string(),
                by: bookrack_config::LibraryIdentification::Path,
            }
        })
        .expect("the flag sugars");
        assert_eq!(routing, Routing::Named("beta".to_string()));
        assert_eq!(seen.borrow().as_slice(), [PathBuf::from("/roots/beta")]);
    }

    /// An empty environment variable is not a selection. Treating it as
    /// one would put every command in a shell that exported it blank
    /// through a lookup that answers nothing.
    #[test]
    fn an_empty_environment_value_selects_nothing() {
        let routing = routing_with(&LibrarySelection::default(), Some(""), unclaimed)
            .expect("no selection, no lookup");
        assert_eq!(routing, Routing::Unselected);
    }

    /// A root the registry does not carry is reported as unclaimed, not
    /// refused: a library nobody registered is an ordinary setup, and
    /// whether *this* daemon serves it is the daemon's answer to give.
    #[test]
    fn an_unregistered_root_is_unclaimed_rather_than_refused() {
        let selection = LibrarySelection {
            data_dir: Some(PathBuf::from("/roots/stranger")),
            library: None,
        };
        let routing =
            routing_with(&selection, None, unclaimed).expect("an unnamed root is not an error");
        assert_eq!(
            routing,
            Routing::UnclaimedRoot(PathBuf::from("/roots/stranger"))
        );
    }

    /// A uuid registered at another root is refused with both paths in
    /// the message: the registry already knows this library and places
    /// it elsewhere, so neither directory can be acted on without
    /// answering for the other.
    #[test]
    fn a_uuid_registered_elsewhere_names_both_roots() {
        let selection = LibrarySelection {
            data_dir: Some(PathBuf::from("/roots/moved")),
            library: None,
        };
        let err = routing_with(&selection, None, |_| RootIdentity::UuidElsewhere {
            name: "beta".to_string(),
            entry_root: PathBuf::from("/roots/beta"),
        })
        .expect_err("a moved root refuses");
        let BookrackCliError::RootNotRoutable { problem } = err else {
            panic!("wrong variant");
        };
        assert!(problem.summary.contains("/roots/moved"));
        let detail = problem.data.detail.expect("a refusal shows its evidence");
        assert!(
            detail.contains("/roots/beta") && detail.contains("beta"),
            "the detail must name the entry it collided with: {detail}"
        );
    }

    /// The refusal that lands after the daemon answers names both roots
    /// too, and points at the two ways out: register the root, or serve
    /// it.
    #[test]
    fn the_wrong_daemon_refusal_names_both_roots_and_both_ways_out() {
        let problem = serves_another_root(Path::new("/roots/asked"), Path::new("/roots/served"));
        assert!(problem.summary.contains("/roots/asked"));
        let detail = problem.data.detail.expect("evidence");
        assert!(detail.contains("/roots/served"), "{detail}");
        let hint = problem.data.hint.expect("a way out");
        assert!(
            hint.contains("libraries register") && hint.contains("bookrack quit"),
            "both ways out belong in the hint: {hint}"
        );
    }
}
