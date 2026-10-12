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
//! **Eviction is per class and least-recent first, never a `clear()`.** The
//! parent threads and the delegated agents are capped separately, at
//! [`HARD_CAP`] keys each — the two budgets the stage router's pins have — so
//! a delegated fan-out can only ever evict delegated counts, never a parent's.
//! When an unseen key arrives and its class is full, expired entries go first
//! (windowed tables only); if the class is still full, its single least
//! recently charged or touched entry is removed, and that key's count
//! restarts from zero when it next charges. Eviction therefore resets only the
//! most idle session's budget: clearing the map instead would hand every live
//! session — including one that had spent `max_judge_calls` — its whole budget
//! again. The cap is the property that matters — an unbounded map keyed by
//! caller-supplied ids is a memory-growth surface a client controls — and the
//! recency order is what keeps it from being a budget reset a client controls.
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

/// Most keys one table counts per class — parent threads, delegated agents —
/// before the class's least recent entry is evicted.
const HARD_CAP: usize = 4_096;

/// The byte an agent-id-less delegated turn hashes in the agent position.
///
/// `0xFF` never occurs in UTF-8, so no agent id — which is a `&str` — can hash
/// to the same bytes, and neither can the parent, which hashes nothing there.
const UNNAMED_DELEGATE: u8 = 0xFF;

/// The key a pair counts under: the digest `sha256(session ‖ agent)`, and
/// whether the turn was delegated.
///
/// The two are joined with a byte that cannot occur inside a header value, so
/// `("ab", "c")` and `("a", "bc")` are different keys rather than the same
/// concatenation. `delegated` is the eviction class: parents and delegated
/// agents are capped apart, so it rides on the key rather than being re-derived
/// from a digest that no longer holds the scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BudgetKey {
    digest: [u8; 32],
    delegated: bool,
}

impl BudgetKey {
    /// The eviction class this key is capped in: delegated agents or parents.
    pub(super) fn is_delegated(&self) -> bool {
        self.delegated
    }
}

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

/// One key's count, the window it lives in (`None`: no clock), and when it
/// was last charged or touched — the order eviction removes entries in.
#[derive(Debug, Clone, Copy)]
struct Spent {
    calls: u32,
    window: Option<Window>,
    last: Instant,
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
        let scope = agent_id
            .filter(|id| !id.trim().is_empty())
            .map_or(BudgetScope::Parent, BudgetScope::Agent);
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

    /// The session's own key, whatever agent sent the turn: the parent's
    /// digest for the session, or `None` for a sessionless request.
    ///
    /// Not a budget key — no budget counts a delegated turn on it. It is the
    /// key [`super::retention`] holds an escalation latch under, because libsy
    /// keeps that latch in session state keyed by the session id alone, so a
    /// session's parent and its children share it.
    pub(super) fn session_key(hints: &RouterContext<'_>) -> Option<BudgetKey> {
        let session = hints
            .session_id
            .map(str::trim)
            .filter(|id| !id.is_empty())?;
        Some(digest(Sha256::new(), session, BudgetScope::Parent))
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
        let live = used.get(key).filter(|spent| spent.is_live(now)).copied();
        let spent = live.map_or(0, |spent| spent.calls);
        if spent >= max {
            return false;
        }
        // Never backwards: a turn that started before one already charged
        // here carries the earlier instant, and restarting the window from it
        // would expire the count early.
        let window = later(live.and_then(|spent| spent.window), window);
        // The driven lane has no window, so its recency comes from the clock.
        let at = now.unwrap_or_else(Instant::now);
        let last = live.map_or(at, |spent| spent.last.max(at));
        // No class can hold `HARD_CAP` keys while the whole map holds fewer, so
        // the per-class count is only taken once the map is that large.
        if used.len() >= HARD_CAP && !used.contains_key(key) {
            make_room(&mut used, key.delegated, now);
        }
        used.insert(
            *key,
            Spent {
                calls: spent + 1,
                window,
                last,
            },
        );
        true
    }

    /// Refresh a live count's window because its session was served again.
    ///
    /// Only a count that exists and is still live moves: a session that has
    /// made no call has nothing here to keep alive, and an expired count must
    /// stay expired so the next charge starts it over.
    ///
    /// Never backwards: `window.now` is the turn's start, so a slow turn that
    /// commits after a later one would otherwise move the window back to an
    /// instant the count has already outlived and expire it early.
    pub(crate) fn touch(&self, key: &BudgetKey, window: Window) {
        if let Some(spent) = self.lock().get_mut(key) {
            if spent.is_live(Some(window.now)) {
                spent.window = later(spent.window, Some(window));
                spent.last = spent.last.max(window.now);
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

/// `next`, unless `kept` started later: then `kept`'s start with `next`'s TTL.
///
/// A window only ever moves forward. `next` is always the caller's turn start,
/// and turns do not finish in the order they started, so taking it blindly
/// would let a slow turn rewind a window a later turn already advanced.
fn later(kept: Option<Window>, next: Option<Window>) -> Option<Window> {
    match (kept, next) {
        (Some(kept), Some(next)) if kept.now > next.now => Some(Window {
            now: kept.now,
            ttl: next.ttl,
        }),
        _ => next,
    }
}

/// Free one slot in `delegated`'s class if it is at [`HARD_CAP`], for an
/// unseen key about to be inserted into it.
///
/// Expired counts go first (a windowed table only: `now` is `None` for the
/// driven lane, whose counts never expire); if the class is still full, its
/// least recently charged or touched entry goes. The map is never cleared, so
/// no live count is reset but the most idle one of the inserting class.
///
/// The class count and the eviction scan are each O(n) in the map's size.
/// That is acceptable because this runs only for an unseen key once the map
/// holds at least [`HARD_CAP`] entries, and only on a judge-call charge that is
/// immediately followed by the judge's network round trip, which dwarfs a walk
/// over a few thousand entries.
fn make_room(used: &mut HashMap<BudgetKey, Spent>, delegated: bool, now: Option<Instant>) {
    let in_class = |used: &HashMap<BudgetKey, Spent>| {
        used.keys().filter(|key| key.delegated == delegated).count()
    };
    if in_class(used) < HARD_CAP {
        return;
    }
    if now.is_some() {
        used.retain(|_, spent| spent.is_live(now));
        if in_class(used) < HARD_CAP {
            return;
        }
    }
    let idlest = used
        .iter()
        .filter(|(key, _)| key.delegated == delegated)
        .min_by_key(|(_, spent)| spent.last)
        .map(|(key, _)| *key);
    if let Some(idlest) = idlest {
        used.remove(&idlest);
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
    BudgetKey {
        digest: hasher.finalize().into(),
        delegated: scope != BudgetScope::Parent,
    }
}
