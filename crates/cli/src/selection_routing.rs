// SPDX-License-Identifier: Apache-2.0

//! Classifies every command by how an explicit library selection
//! (`--data-dir` / `--library` / `BOOKRACK_DATA_DIR`) reaches it.
//!
//! Two sides:
//!
//!   * **locally resolving** — the command resolves a data root itself
//!     through `Config::resolve`, so the selection is a real switch into
//!     a different root, or an offline registry read/write;
//!   * **daemon-routed** — the command acts through a running session,
//!     so the selection is an assertion about the library that session
//!     serves rather than a switch. [`crate::preflight`] refuses the
//!     invocation when the assertion disagrees with the running daemon.
//!
//! The classification is an exhaustive `match` at every level: a new
//! top-level command, a new `libraries` verb, and a new `index-profile`
//! verb each fail to compile until they are filed on one side. A
//! `matches!` would file them on the daemon-routed side silently.
//!
//! The split is leaf-grained where a namespace spans both sides:
//! `libraries` has seven offline verbs among ten, and `index-profile
//! apply` executes through the daemon unless it is a `--dry-run`.
//! The top-level command names this module mentions are cross-asserted
//! against the surface's own top-level whitelist in `main`'s tests —
//! the two tables have different granularity and evolve separately, so
//! neither is derived from the other.

use bookrack_runtime::cmd::index_profile::IndexProfileAction;

use crate::{Command, LibrariesAction};

/// Whether `command` resolves its own data root, leaving an explicit
/// library selection to act as a switch rather than as an assertion
/// about a running daemon.
///
/// `doctor` is deliberately daemon-routed. It has a daemon-not-running
/// fallback that probes the data root directly, but classifying it as
/// local by that fallback would let a selection naming one library
/// silently diagnose the library a running daemon serves.
pub(crate) fn resolves_root_locally(command: &Command) -> bool {
    match command {
        Command::Init { .. }
        | Command::Run { .. }
        | Command::AuditProfile { .. }
        | Command::Distill { .. }
        | Command::Runs { .. }
        | Command::Retrieval { .. } => true,

        // `apply` executes its plan through the daemon; its `--dry-run`
        // form stays offline.
        Command::IndexProfile { action } => match action {
            IndexProfileAction::Apply { dry_run, .. } => *dry_run,
            IndexProfileAction::List { .. }
            | IndexProfileAction::Show { .. }
            | IndexProfileAction::Validate { .. }
            | IndexProfileAction::Current { .. }
            | IndexProfileAction::Diff { .. } => true,
        },

        // The registry verbs read and write the registry file directly;
        // the three that report on a library go through the session.
        Command::Libraries { action } => match action {
            LibrariesAction::Default { .. }
            | LibrariesAction::Detect { .. }
            | LibrariesAction::Scan { .. }
            | LibrariesAction::Add { .. }
            | LibrariesAction::Register { .. }
            | LibrariesAction::Remove { .. }
            | LibrariesAction::Config { .. } => true,
            LibrariesAction::List { .. }
            | LibrariesAction::Info { .. }
            | LibrariesAction::Fork { .. }
            | LibrariesAction::Mount { .. } => false,
        },

        Command::Config { .. }
        | Command::Verify
        | Command::Diagnose { .. }
        | Command::Rpc { .. }
        | Command::Doctor { .. }
        | Command::Ingest(_)
        | Command::Glean(_)
        | Command::Intake { .. }
        | Command::Queue { .. }
        | Command::Metadata { .. }
        | Command::Vectors { .. }
        | Command::Corpus { .. }
        | Command::Stamps { .. }
        | Command::Remove(_)
        | Command::Papers { .. }
        | Command::Find(_)
        | Command::List(_)
        | Command::Search(_)
        | Command::Show { .. }
        | Command::Dryrun(_)
        | Command::Quit
        | Command::Logs(_)
        | Command::Status => false,
    }
}
