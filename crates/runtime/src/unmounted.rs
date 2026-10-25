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

/// Whether the daemon a reason is worded for is serving, so a hint can
/// close with the retry that fits: a running daemon mounts the repaired
/// library through `libraries mount`, and a bring-up that refused is
/// started again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountOccasion {
    /// The daemon is up: a skip it records, or a runtime
    /// `library.mount`.
    Serving,
    /// Bring-up refused, so there is no daemon to mount through.
    Refusing,
}

impl MountOccasion {
    /// The closing clause of a hint: how to try again once the root
    /// is back.
    pub(crate) fn retry(self, name: &str) -> String {
        match self {
            MountOccasion::Serving => format!("then run `bookrack libraries mount {name}`"),
            MountOccasion::Refusing => String::from("then start the daemon again"),
        }
    }
}

/// One library bring-up skipped: the record the registry keeps, the
/// reason a refused bring-up gives for it when that reads differently,
/// and the error that caused it, kept whole so a bring-up with nothing
/// else to report can return it as it was.
pub(crate) struct Skip {
    pub(crate) library: UnmountedLibrary,
    /// The reason [`NoServableLibrary`] names the library with; `None`
    /// reuses the record's.
    pub(crate) refusal: Option<Problem>,
    pub(crate) cause: eyre::Report,
}

/// The libraries one bring-up skipped, in the order it skipped them.
#[derive(Default)]
pub(crate) struct Skips(Vec<Skip>);

impl Skips {
    /// Record a skip whose reason reads the same whether the daemon
    /// goes on serving or refuses.
    pub(crate) fn push(&mut self, library: UnmountedLibrary, cause: eyre::Report) {
        self.0.push(Skip {
            library,
            refusal: None,
            cause,
        });
    }

    /// Record a skip whose reason, should bring-up end up refusing,
    /// reads as `refusal` rather than as the record a serving daemon
    /// keeps.
    pub(crate) fn push_worded(
        &mut self,
        library: UnmountedLibrary,
        refusal: Problem,
        cause: eyre::Report,
    ) {
        self.0.push(Skip {
            library,
            refusal: Some(refusal),
            cause,
        });
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
    /// [`NoServableLibrary`] naming each with its refusal wording,
    /// sorted by name as the registry lists them rather than in the
    /// order the steps found them.
    pub(crate) fn into_refusal(self) -> eyre::Report {
        let mut skips = self.0;
        if skips.len() == 1 {
            return skips.remove(0).cause;
        }
        let mut libraries: Vec<UnmountedLibrary> = skips
            .into_iter()
            .map(|skip| match skip.refusal {
                Some(reason) => {
                    UnmountedLibrary::new(skip.library.name(), skip.library.data_dir(), reason)
                }
                None => skip.library,
            })
            .collect();
        libraries.sort_by(|a, b| a.name().cmp(b.name()));
        NoServableLibrary { libraries }.into()
    }
}

/// The reason a library whose registry entry does not resolve is not
/// served. A missing data root — the usual case, a volume that is not
/// connected — gets the two ways back, closed with the retry `occasion`
/// calls for; anything else carries the resolution error's own chain.
pub(crate) fn resolve_problem(name: &str, err: &ConfigError, occasion: MountOccasion) -> Problem {
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
             location with `bookrack libraries add {name} <path>`, {}.",
            occasion.retry(name)
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

    /// A refused bring-up names a skip pushed with its own refusal
    /// wording by that wording, and one pushed without by its record;
    /// the records a serving daemon would keep are left as pushed.
    #[test]
    fn a_refused_bring_up_reads_each_skips_refusal_wording() {
        let mut skips = Skips::default();
        skips.push_worded(
            skipped(
                "alpha",
                "no manifest",
                Some("Reconnect it, then run `bookrack libraries mount alpha`."),
                false,
            ),
            Problem::new(String::from("no manifest"))
                .hint(String::from("Reconnect it, then start the daemon again.")),
            eyre::eyre!("first cause"),
        );
        skips.push(
            skipped("beta", "schema too new", Some("Upgrade bookrack."), false),
            eyre::eyre!("second cause"),
        );
        let records: Vec<Option<String>> = skips
            .libraries()
            .map(|library| library.reason().data.hint.clone())
            .collect();
        assert_eq!(
            records,
            [
                Some(String::from(
                    "Reconnect it, then run `bookrack libraries mount alpha`."
                )),
                Some(String::from("Upgrade bookrack.")),
            ]
        );

        let err = skips.into_refusal();
        let refusal = err
            .downcast_ref::<NoServableLibrary>()
            .expect("several skips refuse as one");
        let alpha = &refusal.libraries[0];
        assert_eq!(alpha.name(), "alpha");
        assert_eq!(
            alpha.data_dir(),
            std::path::Path::new("/disk/library/alpha")
        );
        assert_eq!(
            alpha.reason().data.hint.as_deref(),
            Some("Reconnect it, then start the daemon again.")
        );
        assert_eq!(
            refusal.libraries[1].reason().data.hint.as_deref(),
            Some("Upgrade bookrack.")
        );
    }

    /// A missing root names the volume and the two registry commands;
    /// the mount command names the library.
    #[test]
    fn a_missing_root_hints_at_reconnecting_and_mounting() {
        let err = ConfigError::DataDirNotFound(std::path::PathBuf::from("/disk/external/beta"));
        let problem = resolve_problem("beta", &err, MountOccasion::Serving);
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
        assert!(
            hint.ends_with("then run `bookrack libraries mount beta`."),
            "{hint}"
        );

        let refusing = resolve_problem("beta", &err, MountOccasion::Refusing);
        assert_eq!(refusing.summary, problem.summary);
        let hint = refusing.data.hint.expect("hint");
        assert!(hint.ends_with("then start the daemon again."), "{hint}");
        assert!(!hint.contains("libraries mount"), "{hint}");
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
