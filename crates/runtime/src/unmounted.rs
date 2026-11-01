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
use std::path::PathBuf;

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
///
/// The reasons themselves name the repair and no retry; the retry is
/// added once, by [`MountOccasion::close`], where the occasion is known:
/// [`Skips::push`] for bring-up, and the control plane's mapping of a
/// runtime mount refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountOccasion {
    /// The daemon is up: a skip it records, or a runtime
    /// `library.mount`.
    Serving,
    /// Bring-up refused, so there is no daemon to mount through.
    Refusing,
}

impl MountOccasion {
    /// `reason` with the retry this occasion calls for as the last
    /// sentence of its hint. A reason without a hint is returned as it
    /// was: it names no repair for a retry to follow.
    pub(crate) fn close(self, name: &str, mut reason: Problem) -> Problem {
        if let Some(hint) = reason.data.hint.take() {
            let retry = match self {
                MountOccasion::Serving => format!("Then run `bookrack libraries mount {name}`."),
                MountOccasion::Refusing => String::from("Then start the daemon again."),
            };
            reason.data.hint = Some(format!("{hint} {retry}"));
        }
        reason
    }
}

/// One library bring-up skipped: the record the registry keeps, the
/// reason a refused bring-up gives for it, and the error that caused
/// it, kept whole so a bring-up with nothing else to report can return
/// it as it was.
pub(crate) struct Skip {
    pub(crate) library: UnmountedLibrary,
    /// The reason [`NoServableLibrary`] names the library with.
    pub(crate) refusal: Problem,
    pub(crate) cause: eyre::Report,
}

/// The libraries one bring-up skipped, in the order it skipped them.
#[derive(Default)]
pub(crate) struct Skips(Vec<Skip>);

impl Skips {
    /// Record that bring-up skipped `name`, registered at `data_dir`,
    /// for `reason`, whose hint names the repair and no retry. The
    /// record a serving daemon keeps closes the hint with the mount;
    /// the reason a refused bring-up gives closes it with starting the
    /// daemon again.
    pub(crate) fn push(
        &mut self,
        name: &str,
        data_dir: impl Into<PathBuf>,
        reason: Problem,
        cause: eyre::Report,
    ) {
        self.0.push(Skip {
            library: UnmountedLibrary::new(
                name,
                data_dir,
                MountOccasion::Serving.close(name, reason.clone()),
            ),
            refusal: MountOccasion::Refusing.close(name, reason),
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
            .map(|skip| {
                UnmountedLibrary::new(skip.library.name(), skip.library.data_dir(), skip.refusal)
            })
            .collect();
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
             location with `bookrack libraries add {name} <path>`."
        )),
        other => Problem::from_error_chain(other),
    }
}

/// The error written onto a queued job whose library was not served at
/// bring-up: the fact alone, which a failure listing groups rows by.
/// The repair is [`queue_failure_problem`]'s hint.
pub(crate) fn queue_failure_message(library: &UnmountedLibrary) -> String {
    format!(
        "library {:?} was not mounted at bring-up: {}",
        library.name(),
        library.reason().summary
    )
}

/// The problem written onto such a job: the skip's record, its hint
/// followed by the retry to run once the library is mounted. A record
/// without a hint gets the mount and the retry as the whole hint.
pub(crate) fn queue_failure_problem(library: &UnmountedLibrary) -> Problem {
    let mut problem = library.reason().clone();
    problem.data.hint = Some(match problem.data.hint.take() {
        Some(hint) => format!("{hint} Once it is mounted, run `bookrack queue retry`."),
        None => format!(
            "Run `bookrack libraries mount {}`, then `bookrack queue retry`.",
            library.name()
        ),
    });
    problem
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
            "alpha",
            "/disk/library/alpha",
            Problem::new("root locked"),
            eyre::eyre!("lock the data root of library 'alpha'"),
        );
        let err = one.into_refusal();
        assert!(err.downcast_ref::<NoServableLibrary>().is_none());
        assert!(format!("{err}").contains("lock the data root"), "{err}");

        let mut two = Skips::default();
        two.push(
            "alpha",
            "/disk/library/alpha",
            Problem::new("root locked"),
            eyre::eyre!("first cause"),
        );
        two.push(
            "beta",
            "/disk/library/beta",
            Problem::new("schema too new"),
            eyre::eyre!("second cause"),
        );
        let err = two.into_refusal();
        let refusal = err
            .downcast_ref::<NoServableLibrary>()
            .expect("several skips refuse as one");
        assert_eq!(refusal.libraries.len(), 2);
        assert_eq!(refusal.libraries[1].name(), "beta");
    }

    /// A skip's record closes its hint with the mount a serving daemon
    /// takes, and a refused bring-up names the library by the same
    /// reason closed with starting the daemon again. A reason without a
    /// hint is left without one on both sides.
    #[test]
    fn a_skip_is_closed_once_for_the_record_and_once_for_the_refusal() {
        let mut skips = Skips::default();
        skips.push(
            "alpha",
            "/disk/library/alpha",
            Problem::new("no manifest").hint("Reconnect it."),
            eyre::eyre!("first cause"),
        );
        skips.push(
            "beta",
            "/disk/library/beta",
            Problem::new("beta failed: disk on fire"),
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
                    "Reconnect it. Then run `bookrack libraries mount alpha`."
                )),
                None,
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
            Some("Reconnect it. Then start the daemon again.")
        );
        assert_eq!(refusal.libraries[1].reason().data.hint, None);
        let merged = refusal.explain().data.hint.unwrap_or_default();
        assert!(!merged.contains("mount"), "{merged}");
    }

    /// The retry is the hint's last sentence, worded for the occasion;
    /// the rest of the reason is left as it was.
    #[test]
    fn closing_appends_the_occasions_retry_to_a_hint_and_leaves_none_alone() {
        let reason = Problem::new("cannot lock the root")
            .detail("Held by pid 7.")
            .hint("Stop the other daemon.")
            .retryable(true);
        let serving = MountOccasion::Serving.close("beta", reason.clone());
        assert_eq!(serving.summary, reason.summary);
        assert_eq!(serving.data.detail, reason.data.detail);
        assert!(serving.data.retryable);
        assert_eq!(
            serving.data.hint.as_deref(),
            Some("Stop the other daemon. Then run `bookrack libraries mount beta`.")
        );
        let refusing = MountOccasion::Refusing.close("beta", reason);
        assert_eq!(
            refusing.data.hint.as_deref(),
            Some("Stop the other daemon. Then start the daemon again.")
        );

        let bare = Problem::new("beta failed: disk on fire");
        for occasion in [MountOccasion::Serving, MountOccasion::Refusing] {
            assert_eq!(occasion.close("beta", bare.clone()), bare);
        }
    }

    /// A missing root names the volume and the two registry commands,
    /// and leaves the retry to the occasion.
    #[test]
    fn a_missing_root_hints_at_reconnecting_and_registering() {
        let err = ConfigError::DataDirNotFound(std::path::PathBuf::from("/disk/external/beta"));
        let problem = resolve_problem("beta", &err);
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
            hint.ends_with("`bookrack libraries add beta <path>`."),
            "{hint}"
        );
        assert!(!hint.contains("libraries mount"), "{hint}");
    }

    /// The error a failed row groups by states the fact; the mount and
    /// the retry are the problem's hint, the mount said once.
    #[test]
    fn a_queued_jobs_failure_states_the_fact_and_hints_the_two_commands() {
        let record = skipped(
            "beta",
            "schema too new",
            Some("Upgrade bookrack. Then run `bookrack libraries mount beta`."),
            false,
        );
        let message = queue_failure_message(&record);
        assert_eq!(
            message,
            "library \"beta\" was not mounted at bring-up: schema too new"
        );
        let problem = queue_failure_problem(&record);
        assert_eq!(problem.summary, "schema too new");
        assert_eq!(
            problem.data.hint.as_deref(),
            Some(
                "Upgrade bookrack. Then run `bookrack libraries mount beta`. Once it is \
                 mounted, run `bookrack queue retry`."
            )
        );

        let bare = skipped("beta", "beta failed: disk on fire", None, false);
        assert_eq!(
            queue_failure_problem(&bare).data.hint.as_deref(),
            Some("Run `bookrack libraries mount beta`, then `bookrack queue retry`.")
        );
    }
}
