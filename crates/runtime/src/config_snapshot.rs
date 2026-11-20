// SPDX-License-Identifier: Apache-2.0

//! The configuration a daemon resolved for itself, as taken at start.
//!
//! A row here is what the running process uses. A later edit to the
//! environment of some shell, or to a `.env`, changes what a restart
//! would resolve and leaves this untouched.

use std::collections::BTreeSet;

use bookrack_config::{DaemonConfig, ENV_PREFIX, RETIRED_ENV_VARS};
use bookrack_core::knob::KnobOrigin;
use serde::Serialize;

/// Where a process got a variable, serialized as its token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VariableSource {
    /// The process started with it.
    Environment,
    /// The process's `.env` supplied it.
    Dotenv,
}

/// One retired variable the daemon's environment carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetiredSeen {
    /// The variable's name.
    pub key: &'static str,
    /// Where the daemon got it.
    pub source: VariableSource,
    /// What replaces it.
    pub help: &'static str,
}

/// One prefixed variable the daemon's environment carried that no knob
/// reads. The value is left out: nothing knows what it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnclaimedSeen {
    /// The variable's name.
    pub key: String,
    /// Where the daemon got it.
    pub source: VariableSource,
}

/// The process-scoped half of `daemon.config`.
#[derive(Debug, Clone, Default)]
pub struct ProcessSnapshot {
    /// The `.env` the daemon loaded, when it loaded one.
    pub dotenv_path: Option<String>,
    /// Every process- and machine-scoped row.
    pub rows: Vec<KnobOrigin>,
    /// The retired variables the daemon carried, in table order.
    pub retired_variables: Vec<RetiredSeen>,
    /// The prefixed variables no knob reads, sorted by name. `None`
    /// when the host supplied no inventory to subtract: not computed,
    /// which is a different answer from an empty list.
    pub unclaimed_variables: Option<Vec<UnclaimedSeen>>,
    /// The libraries mounted when the daemon entered service.
    pub mounted_at_start: Vec<String>,
}

impl ProcessSnapshot {
    /// Take the snapshot from the configuration the daemon resolved.
    ///
    /// The rows are that configuration's own, then the two that decide
    /// how libraries are found, then each lower-tier crate's report.
    /// `claimed` is every variable some knob of the build reads.
    pub fn capture(
        daemon: &DaemonConfig,
        claimed: Option<&BTreeSet<String>>,
        mounted_at_start: Vec<String>,
    ) -> ProcessSnapshot {
        let env = daemon.env();
        let source = |name: &str| match env.dotenv_load() {
            Some(load) if load.supplied.iter().any(|supplied| supplied == name) => {
                VariableSource::Dotenv
            }
            _ => VariableSource::Environment,
        };

        let mut rows = daemon.origins().to_vec();
        rows.extend(bookrack_config::selection_origins(env));
        for contributor in bookrack_ops::settings::KNOB_CONTRIBUTORS {
            rows.extend((contributor.report)(env.dotenv()));
        }

        let retired_variables = daemon
            .retired_seen()
            .iter()
            .map(|var| RetiredSeen {
                key: var.name,
                source: source(var.name),
                help: var.help,
            })
            .collect();
        let unclaimed_variables = claimed.map(|claimed| {
            env.names()
                .filter(|name| name.starts_with(ENV_PREFIX) && !claimed.contains(*name))
                .filter(|name| !RETIRED_ENV_VARS.iter().any(|var| var.name == *name))
                .map(|name| UnclaimedSeen {
                    key: name.to_string(),
                    source: source(name),
                })
                .collect()
        });

        ProcessSnapshot {
            dotenv_path: env.dotenv_load().map(|load| load.supply().path.to_string()),
            rows,
            retired_variables,
            unclaimed_variables,
            mounted_at_start,
        }
    }
}

#[cfg(test)]
mod tests {
    use bookrack_config::{DaemonFlags, EnvSnapshot};

    use super::*;

    fn daemon(pairs: &[(&str, &str)]) -> DaemonConfig {
        DaemonConfig::resolve(
            &EnvSnapshot::from_pairs(pairs.iter().copied()),
            &DaemonFlags::default(),
        )
    }

    #[test]
    fn a_host_that_supplies_no_claimed_set_reports_null_not_empty() {
        let daemon = daemon(&[("BOOKRACK_TYPO", "1")]);
        let uncomputed = ProcessSnapshot::capture(&daemon, None, Vec::new());
        assert_eq!(uncomputed.unclaimed_variables, None);

        let computed = ProcessSnapshot::capture(&daemon, Some(&BTreeSet::new()), Vec::new());
        assert_eq!(
            computed.unclaimed_variables,
            Some(vec![UnclaimedSeen {
                key: "BOOKRACK_TYPO".to_string(),
                source: VariableSource::Environment,
            }])
        );
    }

    #[test]
    fn a_claimed_or_retired_variable_is_not_unclaimed() {
        let daemon = daemon(&[
            ("BOOKRACK_OLLAMA_URL", "http://h:1"),
            ("BOOKRACK_VECTORS_NPROBES", "8"),
            ("UNPREFIXED", "1"),
        ]);
        let claimed = BTreeSet::from(["BOOKRACK_OLLAMA_URL".to_string()]);
        let snapshot = ProcessSnapshot::capture(&daemon, Some(&claimed), Vec::new());
        assert_eq!(snapshot.unclaimed_variables, Some(Vec::new()));
        let retired: Vec<&str> = snapshot
            .retired_variables
            .iter()
            .map(|var| var.key)
            .collect();
        assert_eq!(retired, ["BOOKRACK_VECTORS_NPROBES"]);
    }
}
