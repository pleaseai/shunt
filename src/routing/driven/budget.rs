//! `max_judge_calls` for the driven lane, counted per `(session, agent)`
//! (ADR-0005 §3).
//!
//! The pure lane's budget rides the session pin, which the
//! [`StageRouterStore`](crate::routing::stage::StageRouterStore) already keeps.
//! A driven entry has no pin — libsy owns the session state — so the count
//! lives here, beside the algorithm it bounds and with the same lifetime: one
//! per [`DrivenEntry`](super::DrivenEntry), rebuilt by a reload exactly as the
//! affinity map inside the algorithm is.
//!
//! **Keyed by the digest, never the id.** The session id and the agent id are
//! the caller's, and a map keyed by them would hold the conversation's
//! identifiers in process memory for as long as the config lives; the digest
//! answers the only question asked of the key ("is this the same pair?")
//! without keeping anything an operator could read back.
//!
//! **Eviction is a `clear()`, not an LRU.** At [`HARD_CAP`] entries the whole
//! map is dropped. That is deliberately the crudest policy that is still
//! bounded: the value is a small counter whose worst case on eviction is one
//! extra judge call for a session already in flight, so an insertion-ordered
//! queue or a recency heap would buy accuracy nobody can spend at the cost of
//! a second structure to keep consistent under the same lock. The cap is the
//! property that matters — an unbounded map keyed by caller-supplied ids is a
//! memory-growth surface a client controls.
//!
//! **Sessionless requests are not tracked.** A request with no
//! `x-claude-code-session-id` cannot be told from the next one, so counting it
//! would either charge every anonymous caller to one shared bucket or key on
//! nothing at all. It gets the per-request allowance instead: the budget is
//! checked and found empty, the drive runs, and nothing is written.

use std::collections::HashMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// Most `(session, agent)` pairs one entry counts before the map is dropped.
const HARD_CAP: usize = 4_096;

/// The digest a pair keys on: `sha256(session ‖ agent)`.
///
/// The two are joined with a byte that cannot occur inside a header value, so
/// `("ab", "c")` and `("a", "bc")` are different keys rather than the same
/// concatenation.
pub(crate) type BudgetKey = [u8; 32];

/// Judge calls made per `(session, agent)` for one driven entry.
#[derive(Debug, Default)]
pub(crate) struct JudgeBudget {
    used: Mutex<HashMap<BudgetKey, u32>>,
}

impl JudgeBudget {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// The key for one turn, or `None` for a request that carries no session
    /// id and is therefore not tracked.
    pub(crate) fn key(session_id: Option<&str>, agent_id: Option<&str>) -> Option<BudgetKey> {
        let session = session_id.map(str::trim).filter(|id| !id.is_empty())?;
        let mut hasher = Sha256::new();
        hasher.update(session.as_bytes());
        hasher.update([0u8]);
        hasher.update(agent_id.unwrap_or_default().as_bytes());
        Some(hasher.finalize().into())
    }

    /// Calls already made for this key. `0` for an untracked request, which is
    /// what gives it the per-request allowance.
    pub(crate) fn used(&self, key: Option<&BudgetKey>) -> u32 {
        let Some(key) = key else {
            return 0;
        };
        self.lock().get(key).copied().unwrap_or(0)
    }

    /// Reserve one call against `max`, or refuse when this key has none left.
    ///
    /// **Check and increment happen under one lock.** A `used()` read followed
    /// by a separate `charge()` write is two critical sections with the judge's
    /// round trip between them, so two turns racing on the same key could both
    /// see the last slot free and both spend it — overshooting the ceiling by
    /// one per racing turn. That is the bound ADR-0005 §3 puts on a session, so
    /// the reservation is the write that decides it.
    ///
    /// Reserving per `CallModel` rather than per drive is also what bounds an
    /// algorithm that *chains* judges — the composite and subagents-classifier
    /// forms — inside a single drive: the second call of a drive whose budget
    /// is down to one is refused here rather than counted after the fact.
    ///
    /// An untracked request (no session id) is always admitted: it has no key
    /// to accumulate against, and the per-request allowance described in the
    /// module docs is what it gets instead.
    pub(crate) fn try_charge(&self, key: Option<&BudgetKey>, max: u32) -> bool {
        let Some(key) = key else {
            return true;
        };
        let mut used = self.lock();
        if used.get(key).copied().unwrap_or(0) >= max {
            return false;
        }
        if used.len() >= HARD_CAP && !used.contains_key(key) {
            // Documented above: the cap is the property, not the eviction
            // order. Cleared before the insert so the new key is the survivor.
            used.clear();
        }
        *used.entry(*key).or_insert(0) += 1;
        true
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<BudgetKey, u32>> {
        self.used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Entries currently held, for the eviction test.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// The cap, for the eviction test.
    #[cfg(test)]
    pub(crate) const fn hard_cap() -> usize {
        HARD_CAP
    }
}
