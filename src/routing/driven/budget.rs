//! `max_judge_calls`, counted per `(session, agent)` (ADR-0005 §3) — the one
//! counter every lane charges.
//!
//! **One rule, every lane.** A call is charged at the moment it is dispatched,
//! by [`JudgeBudget::try_charge`] (or [`JudgeBudget::try_charge_within`]): the
//! read and the reservation are one lock acquisition with no `.await` between
//! them, so concurrent turns of one session cannot each see the last slot free.
//! A reserved call is never refunded — whatever later happens to the turn that
//! made it — and a turn that makes no call never reaches this table, so the
//! budget cannot refuse it. The driven lane reserves inside libsy's call
//! closure; the stage router's judge reserves immediately before
//! `routing::judge::consult` (issues #634, #648).
//!
//! **Not a routing pin.** The stage router used to park its count on the
//! session's tier pin, which is why the count could only be written when the
//! pin was, after the judge's round trip — and why reserving it earlier would
//! have meant publishing a provisional tier. This table holds counts and
//! nothing else, so a reservation is invisible to tier resolution, dwell,
//! stickiness, and flip accounting. The stage router keeps one of these beside
//! its pins on the [`StageRouterStore`](crate::routing::stage::StageRouterStore);
//! each [`DrivenEntry`](super::DrivenEntry) keeps its own, rebuilt by a reload
//! exactly as the affinity map inside the algorithm is.
//!
//! **Keyed by the digest, never the id.** The session id and the agent id are
//! the caller's, and a map keyed by them would hold the conversation's
//! identifiers in process memory for as long as the config lives; the digest
//! answers the only question asked of the key ("is this the same pair?")
//! without keeping anything an operator could read back.
//!
//! **Liveness is the charger's.** A driven entry's counts have no clock: they
//! live as long as the entry. The stage router's carry the window its pins
//! have — `session_ttl_seconds`, refreshed by every served turn of the session
//! ([`JudgeBudget::touch`]) — and its key covers the router table, so a resumed
//! or reconfigured session gets its budget back exactly when it would have got
//! a fresh pin, without the count riding on one. An expired count reads as
//! zero and is overwritten by the next charge.
//!
//! **Eviction is a sweep then a `clear()`, not an LRU.** At [`HARD_CAP`]
//! entries the expired ones are dropped first, and if that frees nothing the
//! whole map is. That is deliberately the crudest policy that is still
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
//! checked and found empty, the drive runs, and nothing is written here. The
//! allowance itself is still `max_judge_calls` — `drive::reserve` counts a
//! keyless drive's calls in a counter that lives as long as that one drive,
//! so a chaining algorithm cannot spend past the ceiling by having no key.
//! The stage router consults at most once per turn, so its keyless turn gets
//! that one call.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::routing::context::RouterContext;

/// Most `(session, agent)` pairs one table counts before it is swept.
const HARD_CAP: usize = 4_096;

/// The byte an agent-id-less delegated turn hashes in the agent position.
///
/// `0xFF` never occurs in UTF-8, so no agent id — which is a `&str` — can hash
/// to the same bytes, and neither can the parent, which hashes nothing there.
const UNNAMED_DELEGATE: u8 = 0xFF;

/// The digest a pair keys on: `sha256(session ‖ agent)`.
///
/// The two are joined with a byte that cannot occur inside a header value, so
/// `("ab", "c")` and `("a", "bc")` are different keys rather than the same
/// concatenation.
pub(crate) type BudgetKey = [u8; 32];

/// Whose budget a turn draws on, within one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetScope<'a> {
    /// The session's own thread.
    Parent,
    /// A delegated turn that named itself: one budget per agent id.
    Agent(&'a str),
    /// A delegated turn — request class `subagent` or `workflow` — that sent
    /// no non-blank agent id (issue #649).
    ///
    /// Not the parent: these turns are not the parent's work, libsy retains no
    /// affinity for them (there is no identity to key on), so they can consult
    /// on every turn, and a fan-out of them would otherwise spend the parent's
    /// whole budget. Not untracked either, which would leave them unbounded.
    /// So all of a session's unnamed delegates share one budget of their own.
    UnnamedDelegate,
}

impl<'a> BudgetScope<'a> {
    /// The scope [`RouterContext`]'s delegation rule assigns this turn: the
    /// same `is_delegated` every lane partitions pins and libsy affinity by.
    pub(crate) fn of(hints: &RouterContext<'a>) -> Self {
        if !hints.is_delegated() {
            return Self::Parent;
        }
        hints
            .pin_agent_id()
            .map_or(Self::UnnamedDelegate, |id| Self::Agent(id.trim()))
    }
}

/// The liveness a stage-router count carries: when its session was last
/// served, and how long it may go quiet before the count is forgotten.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    pub now: Instant,
    pub ttl: Duration,
}

/// One key's count, and the window it lives in (`None`: no clock).
#[derive(Debug, Clone, Copy)]
struct Spent {
    calls: u32,
    window: Option<Window>,
}

impl Spent {
    /// Whether this count still applies at `now`: the mirror of the stage
    /// store's `is_live` TTL half, boundary included.
    fn is_live(&self, now: Option<Instant>) -> bool {
        match (self.window, now) {
            (Some(window), Some(now)) => now.saturating_duration_since(window.now) <= window.ttl,
            _ => true,
        }
    }
}

/// Judge calls made per `(session, agent)`.
#[derive(Debug, Default)]
pub(crate) struct JudgeBudget {
    used: Mutex<HashMap<BudgetKey, Spent>>,
}

impl JudgeBudget {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The key for one turn, or `None` for a request that carries no session
    /// id and is therefore not tracked.
    ///
    /// `agent_id` names a child's own budget; `None` — and a blank id — is the
    /// parent's. A caller that knows whether the turn was delegated uses
    /// [`JudgeBudget::key_for`] instead, which keeps an unnamed delegate off
    /// the parent's budget.
    #[cfg(test)]
    pub(crate) fn key(session_id: Option<&str>, agent_id: Option<&str>) -> Option<BudgetKey> {
        let session = session_id.map(str::trim).filter(|id| !id.is_empty())?;
        let scope = agent_id.map_or(BudgetScope::Parent, BudgetScope::Agent);
        Some(digest(Sha256::new(), session, scope))
    }

    /// The key a driven entry's budget counts this turn under, or `None` for a
    /// request that carries no session id.
    pub(crate) fn key_for(hints: &RouterContext<'_>) -> Option<BudgetKey> {
        let session = hints
            .session_id
            .map(str::trim)
            .filter(|id| !id.is_empty())?;
        Some(digest(Sha256::new(), session, BudgetScope::of(hints)))
    }

    /// The key the stage router's shared table counts a turn under.
    ///
    /// One table serves every router-backed model and survives a reload, so the
    /// key also covers the advertised model id and the router table's
    /// fingerprint: two models keep separate budgets, and a reload that changes
    /// the table starts the session's count again, as it starts a fresh pin.
    /// `session` is taken as the pin takes it — the caller has already dropped
    /// a sessionless turn.
    pub(crate) fn stage_key(
        model: &str,
        fingerprint: u64,
        session: &str,
        scope: BudgetScope<'_>,
    ) -> BudgetKey {
        let mut hasher = Sha256::new();
        // Length-prefixed, so the model id cannot run into the fingerprint.
        hasher.update((model.len() as u64).to_le_bytes());
        hasher.update(model.as_bytes());
        hasher.update(fingerprint.to_le_bytes());
        digest(hasher, session, scope)
    }

    /// Calls already made for this key. `0` for an untracked request, which is
    /// what gives it the per-request allowance.
    #[cfg(test)]
    pub(crate) fn used(&self, key: Option<&BudgetKey>) -> u32 {
        let Some(key) = key else {
            return 0;
        };
        self.lock().get(key).map_or(0, |spent| spent.calls)
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
    /// An untracked request (no session id) is always admitted *here*: it has
    /// no key to accumulate against. The per-request allowance described in
    /// the module docs is enforced by `drive::reserve`, which counts such a
    /// drive's calls locally instead of calling this.
    pub(crate) fn try_charge(&self, key: Option<&BudgetKey>, max: u32) -> bool {
        self.try_charge_within(key, max, None)
    }

    /// [`JudgeBudget::try_charge`] for a count that lives in `window`: an
    /// expired count reads as zero, and the reservation restarts the window.
    pub(crate) fn try_charge_within(
        &self,
        key: Option<&BudgetKey>,
        max: u32,
        window: Option<Window>,
    ) -> bool {
        let Some(key) = key else {
            return true;
        };
        let now = window.map(|window| window.now);
        let mut used = self.lock();
        let spent = used
            .get(key)
            .filter(|spent| spent.is_live(now))
            .map_or(0, |spent| spent.calls);
        if spent >= max {
            return false;
        }
        if used.len() >= HARD_CAP && !used.contains_key(key) {
            // Documented above: the cap is the property, not the eviction
            // order. Expired counts go first; if none had, everything does.
            // Either way the new key is inserted after, so it survives.
            if now.is_some() {
                used.retain(|_, spent| spent.is_live(now));
            }
            if used.len() >= HARD_CAP {
                used.clear();
            }
        }
        used.insert(
            *key,
            Spent {
                calls: spent + 1,
                window,
            },
        );
        true
    }

    /// Refresh a live count's window because its session was served again.
    ///
    /// Only a count that exists and is still live moves: a session that has
    /// made no call has nothing here to keep alive, and an expired count must
    /// stay expired so the next charge starts it over.
    pub(crate) fn touch(&self, key: &BudgetKey, window: Window) {
        if let Some(spent) = self.lock().get_mut(key) {
            if spent.is_live(Some(window.now)) {
                spent.window = Some(window);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<BudgetKey, Spent>> {
        self.used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Calls a windowed count holds at `now`, for the stage-store tests.
    #[cfg(test)]
    pub(crate) fn used_at(&self, key: &BudgetKey, now: Instant) -> u32 {
        self.lock()
            .get(key)
            .filter(|spent| spent.is_live(Some(now)))
            .map_or(0, |spent| spent.calls)
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

/// `session ‖ 0x00 ‖ agent`, appended to whatever `hasher` already holds.
///
/// The parent hashes nothing in the agent position, a named agent its id, and
/// an unnamed delegate the one byte no id can contain.
fn digest(mut hasher: Sha256, session: &str, scope: BudgetScope<'_>) -> BudgetKey {
    hasher.update(session.as_bytes());
    hasher.update([0u8]);
    match scope {
        BudgetScope::Parent => {}
        BudgetScope::Agent(id) => hasher.update(id.as_bytes()),
        BudgetScope::UnnamedDelegate => hasher.update([UNNAMED_DELEGATE]),
    }
    hasher.finalize().into()
}
