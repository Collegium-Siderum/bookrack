// SPDX-License-Identifier: Apache-2.0

//! The environment a resolution reads, captured once.
//!
//! A resolver that takes an [`EnvSnapshot`] reads no process state: two
//! resolutions from one snapshot cannot disagree, and a test describes
//! an environment without mutating the process's own.

use std::collections::BTreeMap;

use bookrack_core::knob::DotenvSupply;

use crate::{DotenvLoad, RetiredVariable};

/// The variables a process holds, with the record of what its `.env`
/// load contributed.
#[derive(Debug, Clone, Default)]
pub struct EnvSnapshot {
    vars: BTreeMap<String, String>,
    dotenv: Option<DotenvLoad>,
}

impl EnvSnapshot {
    /// Capture the process environment and this process's dotenv
    /// record. A variable whose name or value is not UTF-8 is left out,
    /// as it is by every reader that goes through `std::env::var`.
    pub fn capture() -> EnvSnapshot {
        EnvSnapshot {
            vars: std::env::vars_os()
                .filter_map(|(key, value)| {
                    Some((key.into_string().ok()?, value.into_string().ok()?))
                })
                .collect(),
            dotenv: crate::dotenv_load().cloned(),
        }
    }

    /// A snapshot holding exactly `pairs` and no dotenv record.
    pub fn from_pairs<K, V>(pairs: impl IntoIterator<Item = (K, V)>) -> EnvSnapshot
    where
        K: Into<String>,
        V: Into<String>,
    {
        EnvSnapshot {
            vars: pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
            dotenv: None,
        }
    }

    /// This snapshot with `name` set to `value`, replacing what it held.
    pub fn with_var(mut self, name: impl Into<String>, value: impl Into<String>) -> EnvSnapshot {
        self.vars.insert(name.into(), value.into());
        self
    }

    /// This snapshot with `load` as its dotenv record. The record moves
    /// which layer a value is credited to, never the value.
    pub fn with_dotenv(mut self, load: DotenvLoad) -> EnvSnapshot {
        self.dotenv = Some(load);
        self
    }

    /// The variable's value as held, blank included.
    pub fn get(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }

    /// The variable's value trimmed, with blank read as unset.
    pub fn trimmed(&self, name: &str) -> Option<String> {
        bookrack_core::knob::trimmed(self.get(name))
    }

    /// Whether the variable is present at all, whatever it holds.
    pub fn is_set(&self, name: &str) -> bool {
        self.vars.contains_key(name)
    }

    /// The names of every variable held, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.vars.keys().map(String::as_str)
    }

    /// The dotenv record in the borrowed form a knob row consults.
    pub fn dotenv(&self) -> Option<DotenvSupply<'_>> {
        self.dotenv.as_ref().map(DotenvLoad::supply)
    }

    /// The dotenv record itself, when a file was loaded.
    pub fn dotenv_load(&self) -> Option<&DotenvLoad> {
        self.dotenv.as_ref()
    }

    /// The retired variables this snapshot holds, in table order.
    pub fn retired_seen(&self) -> Vec<RetiredVariable> {
        crate::retired_seen(|name| self.is_set(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_value_is_set_and_trims_to_nothing() {
        let env = EnvSnapshot::from_pairs([("A", "  "), ("B", " x ")]);
        assert!(env.is_set("A"));
        assert_eq!(env.get("A").as_deref(), Some("  "));
        assert_eq!(env.trimmed("A"), None);
        assert_eq!(env.trimmed("B").as_deref(), Some("x"));
        assert!(!env.is_set("C"));
        assert_eq!(env.names().collect::<Vec<_>>(), ["A", "B"]);
    }

    #[test]
    fn retired_seen_reads_the_snapshot_and_not_the_process() {
        let env = EnvSnapshot::from_pairs([("BOOKRACK_VECTORS_NPROBES", "8")]);
        let names: Vec<&str> = env.retired_seen().iter().map(|var| var.name).collect();
        assert_eq!(names, ["BOOKRACK_VECTORS_NPROBES"]);
        assert!(EnvSnapshot::default().retired_seen().is_empty());
    }
}
