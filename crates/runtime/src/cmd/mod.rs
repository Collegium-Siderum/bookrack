// SPDX-License-Identifier: Apache-2.0

//! Per-command implementations. Each submodule owns one CLI command
//! and exposes a `run` entry point that the `main.rs` dispatch router
//! calls into. The shared loaders (`audit_helpers`, `embed_helpers`,
//! `ops_helpers`) and small utilities (`util`) live one level up so
//! commands depend on them through `crate::*`, not on each other.

pub mod audit_profile;
pub mod corpus;
pub mod diagnose;
pub mod dryrun;
pub mod index_profile;
pub mod ingest;
pub mod intake_ocr;
pub mod libraries;
pub mod metadata;
pub mod papers_corpus;
pub mod papers_dryrun;
pub mod papers_stamps;
pub mod papers_vectors;
pub mod remove;
pub mod remove_paper;
pub mod stamps;
pub mod vectors;
pub mod verify;

#[cfg(test)]
mod tests {
    use bookrack_cli_grammar::help_gate;
    use clap::CommandFactory;

    /// Test-only shell that mounts this module's two subcommand enums
    /// under the names the binary gives them, so the gate walks the
    /// paths the debt list records. Both enums hang here rather than in
    /// their own files: one shell, one entry point, one place to add
    /// the third enum if this module ever grows one.
    #[derive(clap::Parser, Debug)]
    #[command(name = "", no_binary_name = true)]
    struct GateCli {
        #[command(subcommand)]
        command: GateCommand,
    }

    #[derive(clap::Subcommand, Debug)]
    enum GateCommand {
        AuditProfile {
            #[command(subcommand)]
            action: super::audit_profile::AuditProfileAction,
        },
        IndexProfile {
            #[command(subcommand)]
            action: super::index_profile::IndexProfileAction,
        },
    }

    /// The gate runs here as well as in `crates/cli` because the daily
    /// gate narrows to the crate a commit touches: a new action on
    /// either enum does not run the binary's tests, and this crate owns
    /// nine of the tree's leaves.
    #[test]
    fn the_runtime_command_surface_obeys_the_help_gate() {
        let violations =
            help_gate::audit_tree(&GateCli::command(), help_gate::Scope::MirroredEnums);
        assert!(
            violations.is_empty(),
            "the help gate rejected this crate's commands:{}",
            help_gate::report(&violations)
        );
        let defects = help_gate::policy_defects();
        assert!(
            defects.is_empty(),
            "the help gate's own constants are malformed: {defects:?}"
        );
    }
}
