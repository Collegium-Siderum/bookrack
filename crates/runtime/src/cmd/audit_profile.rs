// SPDX-License-Identifier: Apache-2.0

//! `bookrack audit-profile` — list, show, or diff built-in audit
//! profiles. The command does not open the data root: the profiles are
//! compiled into the binary by `bookrack_audit_profile`, so this is a
//! pure reflection surface.

use eyre::{ContextCompat, Result};

use crate::render;

#[derive(clap::Subcommand, Debug)]
pub enum AuditProfileAction {
    /// Print every built-in profile name, one per line.
    List {
        /// Emit machine-readable JSON instead of the plain listing.
        #[arg(long)]
        json: bool,
    },
    /// Pretty-print the effective toggle settings for a named profile.
    Show {
        /// Built-in profile name (`default`, `trust-source`, `strict`).
        name: String,
    },
    /// List the sub-section names that differ between two named profiles
    /// and pretty-print each side's settings for those sections.
    Diff {
        /// First profile name.
        a: String,
        /// Second profile name.
        b: String,
    },
}

pub fn run(action: AuditProfileAction) -> Result<()> {
    match action {
        AuditProfileAction::List { json } => {
            if json {
                render::audit_profile_names_json(bookrack_audit_profile::ALL_BUILT_IN_NAMES);
            } else {
                for name in bookrack_audit_profile::ALL_BUILT_IN_NAMES {
                    println!("{name}");
                }
            }
            Ok(())
        }
        AuditProfileAction::Show { name } => {
            let profile = bookrack_audit_profile::AuditProfile::from_named(&name)
                .with_context(|| format!("unknown profile {name:?}"))?;
            render::audit_profile_show(&name, &profile);
            Ok(())
        }
        AuditProfileAction::Diff { a, b } => {
            let pa = bookrack_audit_profile::AuditProfile::from_named(&a)
                .with_context(|| format!("unknown profile {a:?}"))?;
            let pb = bookrack_audit_profile::AuditProfile::from_named(&b)
                .with_context(|| format!("unknown profile {b:?}"))?;
            render::audit_profile_diff(&a, &pa, &b, &pb);
            Ok(())
        }
    }
}
