// SPDX-License-Identifier: Apache-2.0

//! Libraries bring-up decided not to serve.
//!
//! Each skip is recorded as an [`UnmountedLibrary`] with its reason,
//! and the causes travel beside the records so a bring-up that ends
//! with nothing to serve can refuse with the one cause it had, or name
//! every library when it had several. The wording here reaches
//! operators through `status`, queue rows, and `bookrack run`'s
//! refusal, so it is built from `format!` and `String::from` alone:
//! nothing in this file flattens an error chain into a bare string.

use std::fmt;

use bookrack_config::ConfigError;
use bookrack_core::{Explain, Problem};
use bookrack_ops::registry::UnmountedLibrary;

/// Bring-up found no library it could serve, and had more than one to
/// try: the refusal names each one with its reason.
#[derive(Debug)]
pub struct NoServableLibrary {
    /// Every library bring-up skipped, sorted by name.
    pub libraries: Vec<UnmountedLibrary>,
}

impl fmt::Display for NoServableLibrary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no registered library could be mounted: ")?;
        for (i, library) in self.libraries.iter().enumerate() {
            if i > 0 {
                write!(f, "; ")?;
            }
            write!(f, "{} ({})", library.name(), library.reason().summary)?;
        }
        Ok(())
    }
}

impl std::error::Error for NoServableLibrary {}

impl Explain for NoServableLibrary {
    fn explain(&self) -> Problem {
        let detail = self
            .libraries
            .iter()
            .map(|library| {
                format!(
                    "Library {:?} at '{}': {}.",
                    library.name(),
                    library.data_dir().display(),
                    library.reason().summary
                )
            })
            .collect::<Vec<String>>()
            .join(" ");
        let mut hints: Vec<&str> = Vec::new();
        for library in &self.libraries {
            if let Some(hint) = library.reason().data.hint.as_deref()
                && !hints.contains(&hint)
            {
                hints.push(hint);
            }
        }
        let retryable = !self.libraries.is_empty()
            && self
                .libraries
                .iter()
                .all(|library| library.reason().data.retryable);
        let mut problem = Problem::new("cannot start: no registered library could be mounted")
            .detail(detail)
            .retryable(retryable);
        if !hints.is_empty() {
            problem = problem.hint(hints.join(" "));
        }
        problem
    }
}

/// One library bring-up skipped: the record the registry keeps, and
/// the error that caused it, kept whole so a bring-up with nothing
/// else to report can return it as it was.
pub(crate) struct Skip {
    pub(crate) library: UnmountedLibrary,
    pub(crate) cause: eyre::Report,
}

/// The libraries one bring-up skipped, in the order it skipped them.
#[derive(Default)]
pub(crate) struct Skips(Vec<Skip>);

impl Skips {
    pub(crate) fn push(&mut self, library: UnmountedLibrary, cause: eyre::Report) {
        self.0.push(Skip { library, cause });
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Skip> {
        self.0.iter()
    }

    /// The records, in skip order, for the registry to keep.
    pub(crate) fn libraries(&self) -> impl Iterator<Item = &UnmountedLibrary> {
        self.0.iter().map(|skip| &skip.library)
    }

    /// The error bring-up fails with when it has nothing to serve. A
    /// single skip returns its cause unchanged, so a lock conflict or a
    /// preflight refusal is read by the front end exactly as a
    /// one-library daemon always reported it; several skips become a
    /// [`NoServableLibrary`] naming each, sorted by name as the registry
    /// lists them rather than in the order the steps found them.
    pub(crate) fn into_refusal(self) -> eyre::Report {
        let mut skips = self.0;
        if skips.len() == 1 {
            return skips.remove(0).cause;
        }
        let mut libraries: Vec<UnmountedLibrary> =
            skips.into_iter().map(|skip| skip.library).collect();
        libraries.sort_by(|a, b| a.name().cmp(b.name()));
        NoServableLibrary { libraries }.into()
    }
}

/// The reason a library whose registry entry does not resolve is not
/// served. A missing data root — the usual case, a volume that is not
/// connected — gets the two ways back; anything else carries the
/// resolution error's own chain.
pub(crate) fn resolve_problem(name: &str, err: &ConfigError) -> Problem {
    match err {
        ConfigError::DataDirNotFound(root) => Problem::new(format!(
            "cannot serve library {name:?}: its data root is not an existing directory"
        ))
        .detail(format!(
            "The registry places {name:?} at '{}', which is not a directory.",
            root.display()
        ))
        .hint(format!(
            "Reconnect the volume that holds it, or register the library at its new \
             location with `bookrack libraries add {name} <path>`, then run `bookrack \
             libraries mount {name}`."
        )),
        other => Problem::from_error_chain(other),
    }
}

/// The failure written onto a queued job whose library was not served
/// at bring-up.
pub(crate) fn queue_failure_message(library: &UnmountedLibrary) -> String {
    format!(
        "library {:?} was not mounted at bring-up: {}; fix it, run `bookrack libraries mount \
         {}`, then `bookrack queue retry`",
        library.name(),
        library.reason().summary,
        library.name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skipped(name: &str, summary: &str, hint: Option<&str>, retryable: bool) -> UnmountedLibrary {
        let mut problem = Problem::new(String::from(summary)).retryable(retryable);
        if let Some(hint) = hint {
            problem = problem.hint(String::from(hint));
        }
        UnmountedLibrary::new(name, format!("/disk/library/{name}"), problem)
    }

    /// Several skips name every library, carry each hint once, and
    /// are retryable only when every skip is.
    #[test]
    fn no_servable_library_names_each_and_merges_hints() {
        let refusal = NoServableLibrary {
            libraries: vec![
                skipped("alpha", "root locked", Some("Stop the other daemon."), true),
                skipped("beta", "schema too new", Some("Upgrade bookrack."), false),
                skipped(
                    "gamma",
                    "root locked too",
                    Some("Stop the other daemon."),
                    true,
                ),
            ],
        };
        let problem = refusal.explain();
        assert_eq!(
            problem.summary,
            "cannot start: no registered library could be mounted"
        );
        let detail = problem.data.detail.expect("detail");
        for name in ["alpha", "beta", "gamma"] {
            assert!(detail.contains(&format!("{name:?}")), "{detail}");
        }
        assert!(detail.contains("/disk/library/beta"), "{detail}");
        let hint = problem.data.hint.expect("hint");
        assert_eq!(hint.matches("Stop the other daemon.").count(), 1, "{hint}");
        assert!(hint.contains("Upgrade bookrack."), "{hint}");
        assert!(!problem.data.retryable, "beta's skip is not retryable");

        let text = format!("{refusal}");
        assert!(
            text.starts_with("no registered library could be mounted: "),
            "{text}"
        );
        assert!(text.contains("alpha (root locked)"), "{text}");
        assert!(text.contains("beta (schema too new)"), "{text}");
    }

    /// One skip is reported as its cause, so a front end that
    /// downcasts the cause still finds it; two become the refusal.
    #[test]
    fn a_single_skip_keeps_its_cause_and_several_become_a_refusal() {
        let mut one = Skips::default();
        one.push(
            skipped("alpha", "root locked", None, true),
            eyre::eyre!("lock the data root of library 'alpha'"),
        );
        let err = one.into_refusal();
        assert!(err.downcast_ref::<NoServableLibrary>().is_none());
        assert!(format!("{err}").contains("lock the data root"), "{err}");

        let mut two = Skips::default();
        two.push(
            skipped("alpha", "root locked", None, true),
            eyre::eyre!("first cause"),
        );
        two.push(
            skipped("beta", "schema too new", None, false),
            eyre::eyre!("second cause"),
        );
        let err = two.into_refusal();
        let refusal = err
            .downcast_ref::<NoServableLibrary>()
            .expect("several skips refuse as one");
        assert_eq!(refusal.libraries.len(), 2);
        assert_eq!(refusal.libraries[1].name(), "beta");
    }

    /// A missing root names the volume and the two registry commands;
    /// the mount command names the library.
    #[test]
    fn a_missing_root_hints_at_reconnecting_and_mounting() {
        let problem = resolve_problem(
            "beta",
            &ConfigError::DataDirNotFound(std::path::PathBuf::from("/disk/external/beta")),
        );
        assert!(problem.summary.contains("\"beta\""), "{}", problem.summary);
        assert!(
            problem
                .data
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("/disk/external/beta"))
        );
        let hint = problem.data.hint.expect("hint");
        assert!(hint.contains("Reconnect the volume"), "{hint}");
        assert!(
            hint.contains("bookrack libraries add beta <path>"),
            "{hint}"
        );
        assert!(hint.contains("bookrack libraries mount beta"), "{hint}");
    }

    #[test]
    fn the_queue_message_names_the_library_the_reason_and_the_two_commands() {
        let message = queue_failure_message(&skipped("beta", "schema too new", None, false));
        assert!(
            message.contains("\"beta\" was not mounted at bring-up: schema too new"),
            "{message}"
        );
        assert!(
            message.contains("`bookrack libraries mount beta`"),
            "{message}"
        );
        assert!(message.contains("`bookrack queue retry`"), "{message}");
    }
}
