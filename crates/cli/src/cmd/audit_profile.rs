// SPDX-License-Identifier: Apache-2.0

//! `bookrack audit-profile` — list, show, or diff built-in audit
//! profiles. The command does not open the data root: the profiles are
//! compiled into the binary by `bookrack_audit_profile`, so this is a
//! pure reflection surface.

use anyhow::{Context, Result};

use crate::AuditProfileAction;
use crate::render;

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
