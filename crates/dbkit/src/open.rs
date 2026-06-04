// SPDX-License-Identifier: Apache-2.0

//! The four-state self-check protocol every store's `open()` follows.
//!
//! Each store-owning crate inspects the on-disk state, computes one
//! [`OpenDecision`], and acts on it. The variants name the four possible
//! outcomes the protocol distinguishes:
//!
//! - [`OpenDecision::Match`]: the on-disk state matches this binary; the
//!   open proceeds.
//! - [`OpenDecision::Migrate`]: the on-disk schema is behind this binary's
//!   target and can be advanced in place. Only databases that maintain a
//!   forward-only migration sequence (`catalog.db` today) resolve drift
//!   this way.
//! - [`OpenDecision::Rederive`]: the on-disk content was written under a
//!   stamp set this binary no longer accepts (schema revision, embedding
//!   model, extractor version, …). The store is opaque to the binary
//!   until the caller rebuilds it from sources; the open fails so the
//!   caller can run the matching rebuild command.
//! - [`OpenDecision::Refuse`]: the database is unsafe to open with this
//!   binary and no in-product fix exists. Typical causes are a file
//!   written by a newer build, or — once the reader-version guard lands
//!   — a store whose declared minimum reader version exceeds this
//!   binary's. The open fails; the operator must run a different
//!   binary.
//!
//! [`OpenDecision::Match`] is the only outcome where `open()` proceeds.
//! The other three translate to errors in each store's own error enum;
//! the variant chosen by this decision dictates which error the caller
//! sees, and the `&'static str` reason on [`OpenDecision::Rederive`] and
//! [`OpenDecision::Refuse`] flows into the log line so a triaged crash
//! report names the trigger.

/// The outcome of inspecting a database at `open()` time.
///
/// See the module documentation for the four-state self-check protocol
/// this enum encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenDecision {
    /// The on-disk state matches this binary; the open proceeds.
    Match,
    /// The on-disk schema is at revision `from` and must be advanced to
    /// `to` before the open can proceed. Only emitted by stores that
    /// hold a forward-only migration sequence.
    Migrate {
        /// The `user_version` (or equivalent) the on-disk database
        /// currently carries.
        from: i64,
        /// The revision this binary is built against.
        to: i64,
    },
    /// The on-disk content was written under a stamp set this binary no
    /// longer accepts; a rebuild from sources is required before the
    /// open can proceed. The free-form `reason` names the failing stamp
    /// and is propagated to logs.
    Rederive {
        /// One-line description of which stamp disagrees, in
        /// English and free of operator-private values.
        reason: &'static str,
    },
    /// The database is unsafe to open with this binary and no
    /// in-product fix exists. The free-form `reason` names the
    /// inhibiting condition and is propagated to logs.
    Refuse {
        /// One-line description of why the open is being refused, in
        /// English and free of operator-private values.
        reason: &'static str,
    },
}

impl OpenDecision {
    /// Whether this decision allows the open to proceed unchanged.
    ///
    /// Only [`OpenDecision::Match`] returns `true`; every other
    /// outcome is a directive the caller must resolve before reaching
    /// a usable handle.
    pub fn is_match(self) -> bool {
        matches!(self, OpenDecision::Match)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_is_the_only_variant_that_proceeds() {
        assert!(OpenDecision::Match.is_match());
        assert!(!OpenDecision::Migrate { from: 3, to: 5 }.is_match());
        assert!(
            !OpenDecision::Rederive {
                reason: "schema revision mismatch"
            }
            .is_match()
        );
        assert!(
            !OpenDecision::Refuse {
                reason: "schema written by a newer binary"
            }
            .is_match()
        );
    }

    #[test]
    fn variants_compare_by_payload() {
        // Two `Migrate` decisions naming the same versions are equal;
        // the second hop differing makes them distinct. The protocol
        // relies on this for `assert_eq!` in store-side unit tests.
        assert_eq!(
            OpenDecision::Migrate { from: 3, to: 5 },
            OpenDecision::Migrate { from: 3, to: 5 }
        );
        assert_ne!(
            OpenDecision::Migrate { from: 3, to: 5 },
            OpenDecision::Migrate { from: 4, to: 5 }
        );
    }
}
