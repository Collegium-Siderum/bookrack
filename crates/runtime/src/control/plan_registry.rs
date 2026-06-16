// SPDX-License-Identifier: Apache-2.0

//! Server-held registry of pinned plans for two-phase destructive RPCs.
//!
//! Destructive control-plane methods (e.g. `corpus.rebuild`) run as
//! two RPCs: the first computes a plan, registers it here, and
//! returns the assigned [`PlanId`] to the client; the second presents
//! the same [`PlanId`] and the registry hands back the exact payload
//! the operator already saw, so the execute step acts on the
//! confirmed target set instead of re-deriving from possibly drifted
//! state.
//!
//! Plans are intentionally in-memory only and time-limited: ephemeral
//! drafts awaiting human confirmation, not durable commitments. A
//! daemon restart drops every outstanding plan, and the client is
//! expected to rerun the dry-run leg.
//!
//! Scoping invariants enforced on [`PlanRegistry::take`]:
//!
//! - `kind`: a plan registered for one method cannot be redeemed by
//!   another.
//! - `library`: a plan registered against one library cannot be
//!   redeemed against another.
//! - Single-use: a successful `take` removes the entry; presenting
//!   the same [`PlanId`] again returns [`PlanLookupError::NotFound`],
//!   matching the wire-level appearance of an unknown id.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use uuid::Uuid;

/// Server-issued identifier for a pinned plan.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlanId(String);

impl PlanId {
    /// Borrow the raw string form passed across the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for PlanId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reasons [`PlanRegistry::take`] may refuse to honour a plan id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanLookupError {
    /// No entry with that id, or the entry was already consumed.
    NotFound,
    /// The entry existed but its expiry time has passed.
    Expired,
    /// The entry exists but was registered under a different method.
    KindMismatch {
        expected: &'static str,
        actual: &'static str,
    },
    /// The entry exists but was registered against a different library.
    LibraryMismatch { expected: String, actual: String },
}

struct RegisteredPlan {
    kind: &'static str,
    library: String,
    payload: Vec<u8>,
    expires_at: Instant,
}

/// Default time-to-live for a registered plan: long enough for an
/// operator to read the dry-run output and confirm without a clock
/// race, short enough that meaningful state drift forces a re-plan.
pub const DEFAULT_PLAN_TTL: Duration = Duration::from_secs(15 * 60);

/// Time-limited map of pinned plans, shared across the daemon's
/// dispatcher via [`super::methods::MethodContext`].
pub struct PlanRegistry {
    inner: Mutex<HashMap<PlanId, RegisteredPlan>>,
    ttl: Duration,
}

impl PlanRegistry {
    /// Build a registry with the supplied TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Build a registry with [`DEFAULT_PLAN_TTL`].
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_PLAN_TTL)
    }

    /// Serialize the plan, register it under a freshly minted id, and
    /// opportunistically drop any already-expired entries.
    pub fn register<P: Serialize>(
        &self,
        kind: &'static str,
        library: impl Into<String>,
        plan: &P,
    ) -> Result<PlanId, serde_json::Error> {
        let payload = serde_json::to_vec(plan)?;
        let id = PlanId(format!("plan_{}", Uuid::now_v7()));
        let now = Instant::now();
        let entry = RegisteredPlan {
            kind,
            library: library.into(),
            payload,
            expires_at: now + self.ttl,
        };
        let mut map = self.inner.lock().expect("plan registry mutex poisoned");
        map.retain(|_, p| p.expires_at > now);
        map.insert(id.clone(), entry);
        Ok(id)
    }

    /// Look up, validate, and consume a plan. Returns the serialized
    /// payload bytes the caller registered; the handler is responsible
    /// for `serde_json::from_slice`-ing back into its own plan type.
    ///
    /// On `Expired` the entry is dropped; on `KindMismatch` or
    /// `LibraryMismatch` the entry is preserved so the rightful
    /// caller can still consume it.
    pub fn take(
        &self,
        id: &PlanId,
        expected_kind: &'static str,
        expected_library: &str,
    ) -> Result<Vec<u8>, PlanLookupError> {
        let mut map = self.inner.lock().expect("plan registry mutex poisoned");
        let entry = map.remove(id).ok_or(PlanLookupError::NotFound)?;
        if entry.expires_at <= Instant::now() {
            return Err(PlanLookupError::Expired);
        }
        if entry.kind != expected_kind {
            let actual = entry.kind;
            map.insert(id.clone(), entry);
            return Err(PlanLookupError::KindMismatch {
                expected: expected_kind,
                actual,
            });
        }
        if entry.library != expected_library {
            let actual = entry.library.clone();
            map.insert(id.clone(), entry);
            return Err(PlanLookupError::LibraryMismatch {
                expected: expected_library.to_string(),
                actual,
            });
        }
        Ok(entry.payload)
    }

    /// Drop every expired entry. Called opportunistically by
    /// [`PlanRegistry::register`]; exposed for tests and for callers
    /// that want to force a sweep.
    pub fn sweep_expired(&self) {
        let now = Instant::now();
        self.inner
            .lock()
            .expect("plan registry mutex poisoned")
            .retain(|_, p| p.expires_at > now);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("plan registry mutex poisoned")
            .len()
    }
}

impl Default for PlanRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct DemoPlan {
        targets: Vec<i64>,
    }

    fn demo() -> DemoPlan {
        DemoPlan {
            targets: vec![1, 2, 3],
        }
    }

    #[test]
    fn register_and_take_round_trips_the_payload() {
        let reg = PlanRegistry::new();
        let id = reg.register("corpus.rebuild", "main", &demo()).unwrap();
        let bytes = reg.take(&id, "corpus.rebuild", "main").unwrap();
        let plan: DemoPlan = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(plan, demo());
    }

    #[test]
    fn second_take_after_consume_returns_not_found() {
        let reg = PlanRegistry::new();
        let id = reg.register("corpus.rebuild", "main", &demo()).unwrap();
        let _ = reg.take(&id, "corpus.rebuild", "main").unwrap();
        let err = reg.take(&id, "corpus.rebuild", "main").unwrap_err();
        assert_eq!(err, PlanLookupError::NotFound);
    }

    #[test]
    fn expired_entries_report_expired_and_are_dropped() {
        let reg = PlanRegistry::with_ttl(Duration::from_millis(5));
        let id = reg.register("corpus.rebuild", "main", &demo()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let err = reg.take(&id, "corpus.rebuild", "main").unwrap_err();
        assert_eq!(err, PlanLookupError::Expired);
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn kind_mismatch_preserves_entry_for_rightful_caller() {
        let reg = PlanRegistry::new();
        let id = reg.register("corpus.rebuild", "main", &demo()).unwrap();
        let err = reg.take(&id, "vectors.reembed", "main").unwrap_err();
        assert_eq!(
            err,
            PlanLookupError::KindMismatch {
                expected: "vectors.reembed",
                actual: "corpus.rebuild",
            }
        );
        let bytes = reg.take(&id, "corpus.rebuild", "main").unwrap();
        let plan: DemoPlan = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(plan, demo());
    }

    #[test]
    fn library_mismatch_preserves_entry_for_rightful_caller() {
        let reg = PlanRegistry::new();
        let id = reg.register("corpus.rebuild", "main", &demo()).unwrap();
        let err = reg.take(&id, "corpus.rebuild", "other").unwrap_err();
        assert_eq!(
            err,
            PlanLookupError::LibraryMismatch {
                expected: "other".to_string(),
                actual: "main".to_string(),
            }
        );
        let bytes = reg.take(&id, "corpus.rebuild", "main").unwrap();
        let plan: DemoPlan = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(plan, demo());
    }

    #[test]
    fn sweep_expired_drops_only_expired_entries() {
        let reg = PlanRegistry::with_ttl(Duration::from_millis(5));
        let stale = reg.register("corpus.rebuild", "main", &demo()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let reg_long = PlanRegistry::new();
        // Migrate the second registration to a separately-TTL'd
        // registry to keep the assertion deterministic. The point of
        // sweep_expired is the negative space: it does not touch
        // live entries, and after a sweep only live entries remain.
        let live = reg_long
            .register("corpus.rebuild", "main", &demo())
            .unwrap();
        reg.sweep_expired();
        reg_long.sweep_expired();
        assert_eq!(reg.len(), 0);
        assert_eq!(reg_long.len(), 1);
        // The expired id is gone; the live id is still consumable.
        assert_eq!(
            reg.take(&stale, "corpus.rebuild", "main").unwrap_err(),
            PlanLookupError::NotFound
        );
        let _ = reg_long.take(&live, "corpus.rebuild", "main").unwrap();
    }

    #[test]
    fn opportunistic_sweep_runs_on_register() {
        let reg = PlanRegistry::with_ttl(Duration::from_millis(5));
        let _stale = reg.register("corpus.rebuild", "main", &demo()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let _fresh = reg.register("corpus.rebuild", "main", &demo()).unwrap();
        // After the second register, the expired entry is gone; only
        // the fresh one remains.
        assert_eq!(reg.len(), 1);
    }
}
