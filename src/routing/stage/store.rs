//! Session-scoped hysteresis for stage-router decisions.
//!
//! The scorer judges one turn at a time, but a tier flip is not free. Anthropic
//! prompt caching is keyed per model, so a flip forfeits the warm prefix and
//! pays a full cache write on the new tier; the Codex transport hashes `model`
//! into its continuation signature, so a flip also forces a full-input re-send
//! (`src/adapters/responses/codex_continuation.rs`); and a cross-provider flip
//! abandons the session's pooled socket and sticky account slot besides. On a
//! long Claude Code session, routing each turn independently can therefore cost
//! more than never routing at all.
//!
//! So decisions are pinned per session and the two directions are not
//! symmetric: escalation is the cheap direction and fires as soon as the signals
//! clear the threshold, while de-escalation must also outlast a dwell window and
//! clear a stricter threshold.
//!
//! Pins are scoped per agent, not only per session: a `Task` child sends its
//! parent's session id with its own agent id, and keys to an entry of its own
//! ([`entries::SessionKey`]) with its own eviction budget
//! ([`entries::MAX_TRACKED_CHILD_PINS`]). The entry also carries the
//! compaction latch ([`entries::StageSession::compacted`]), which is the one
//! thing a pin holds that is read *before* the turn is scored rather than
//! after.

mod decide;
mod entries;

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use super::{StageDecision, StageSource, StageTier};
use crate::config::StageRouterConfig;
use crate::routing::context::RouterContext;
use crate::routing::driven::budget::{BudgetKey, BudgetScope, JudgeBudget, Window};
use crate::routing::random::DrawState;
use decide::{evict, fingerprint, is_live, resolve};
use entries::{session_key, Entries, PinScope, SessionKey, StageSession};

// Re-exported for the two builds that actually read the caps: `tests` below
// (via `use super::*`) and `bench_support` behind the `bench` feature. The
// default build compiles neither, and an ungated re-export there is an
// `unused_imports` error under CI's `-D warnings` (the `Test default build
// (no ui feature)` job), which `--all-features` runs cannot see.
#[cfg(any(test, feature = "bench"))]
pub(crate) use entries::{MAX_TRACKED_CHILD_PINS, MAX_TRACKED_SESSIONS};

/// A pin [`StageRouterStore::apply`] prepared but has not written.
///
/// Deciding and recording are separate because routing runs *before* inbound
/// auth and the managed-model policy — `check_inbound_auth` needs the resolved
/// chain, so it cannot run first. Committing inside `apply` therefore let a
/// request that was about to be rejected create or evict pins, and let a caller
/// who guessed another client's session id steer that client's next turn
/// without ever presenting a credential. The decision still happens against the
/// pin it read; only the write-back is deferred, to the point where the request
/// is known to be served.
#[derive(Debug)]
pub(crate) struct PendingPin {
    key: SessionKey,
    session: StageSession,
    /// Whether *this* request carried `x-claude-code-context-compacted`, as
    /// opposed to inheriting the latch from the pin it read.
    ///
    /// Only the header itself may re-latch a write that lost the `seq` race.
    /// An inherited flag describes the pin as it was when this turn decided,
    /// and that pin can expire while the turn is in flight — re-latching from
    /// it would write a dead latch onto whatever fresh entry replaced it, for
    /// another full TTL, and repeat for as long as turns keep overlapping.
    observed_compaction: bool,
    /// The session's judge budget, whose window `commit` refreshes because the
    /// session was served. Only the refresh rides here — the count itself is
    /// charged and kept in [`StageRouterStore::try_charge_judge`]'s table,
    /// never on the pin.
    budget: Option<StageBudget>,
}

impl PendingPin {
    /// Overwrite the tier this pin will record, after a judge verdict moved it.
    ///
    /// Resets `dwell_turns` to 1 when the tier actually changes from what
    /// [`StageRouterStore::apply`] decided, for the same reason `apply` restarts
    /// the window on a flip: `min_dwell_turns` counts turns *served at this
    /// tier*, and carrying an inherited count onto a tier this turn is the
    /// first to serve would let the next turn move again immediately. A verdict
    /// that agrees with the estimate changes nothing and keeps the count.
    pub(crate) fn set_tier(&mut self, tier: StageTier) {
        if self.session.tier != tier {
            self.session.tier = tier;
            self.session.dwell_turns = 1;
        }
    }
}

/// Where one session's stage-router judge calls are counted (ADR-0005 §3).
///
/// A key into the store's [`JudgeBudget`], not a count: the count lives in that
/// side table rather than on the tier pin, so reserving a call publishes no
/// tier — a concurrent turn resolving its tier reads the pin map alone and
/// cannot see a reservation, and `commit` has nothing budget-related to decide
/// (issue #634). `ttl` is the router's `session_ttl_seconds`, carried per key
/// for the reason [`entries::StageSession::ttl`] is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StageBudget {
    key: BudgetKey,
    ttl: Duration,
}

impl StageBudget {
    fn window(self, now: Instant) -> Window {
        Window { now, ttl: self.ttl }
    }
}

/// What one [`StageRouterStore::apply`] call decided, and what it displaced.
pub(crate) struct StageApplied {
    pub decision: StageDecision,
    /// The pin this turn earned, parked until the request is admitted. `None`
    /// for a turn that records nothing: no session id, or a read-only probe.
    pub pin: Option<PendingPin>,
    /// Where this turn's judge call would be charged, for a router with a
    /// `[models.router.classifier]` and a session id. `None` otherwise — and a
    /// sessionless turn's `None` is what gives it the one call a turn may make,
    /// since it has no session to accumulate a budget under.
    pub judge_budget: Option<StageBudget>,
}

impl StageApplied {
    /// A decision that records no pin, and so can displace none.
    fn stateless(decision: StageDecision) -> Self {
        Self {
            decision,
            pin: None,
            judge_budget: None,
        }
    }
}

/// Per-session tier pins. Lives on `AppState` beside the account pool, so it
/// survives a config reload rather than being rebuilt by one.
#[derive(Debug, Default)]
pub(crate) struct StageRouterStore {
    entries: Mutex<Entries>,
    /// Hands out the `seq` above. Monotonic for the process, so it orders
    /// decisions across every session and router without a per-key counter.
    next_seq: AtomicU64,
    /// Per-router RNG streams for `type = "random"` draws.
    ///
    /// Here rather than on `AppState` directly because it has the same
    /// lifetime and the same reason for it: it must survive a config reload,
    /// and rebuilding it per reload would restart every seeded sequence.
    draws: DrawState,
    /// `max_judge_calls` for every router's `[models.router.classifier]`,
    /// beside the pins rather than on them (issue #634). Here for the same
    /// reason the pins are: it must survive a reload that leaves the table
    /// alone, and its keys cover the table so a reload that changes it does
    /// not carry a stale count forward.
    judge_budget: JudgeBudget,
}

impl StageRouterStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The next draw for one `type = "random"` entry.
    ///
    /// `fingerprint` covers the selection inputs, so a reload that changes
    /// `targets`, `weights`, or `seed` reseeds rather than continuing a stream
    /// drawn against a different distribution.
    pub(crate) fn random_draw(&self, model: &str, fingerprint: u64, seed: Option<u64>) -> u64 {
        self.draws.next(model, fingerprint, seed)
    }

    /// Apply hysteresis to one turn and return the tier that serves it.
    ///
    /// `hints` carries the request's `x-claude-code-*` headers. A caller that
    /// sends no session id (Claude Code always does; a bare `curl` does not)
    /// is scored statelessly and never touches the store. A delegated turn —
    /// see [`RouterContext::is_delegated`] — keys to a pin of its own, so a
    /// `Task` child neither reads nor overwrites its parent's.
    ///
    /// `estimate` scores the turn. It is a closure rather than a value because
    /// one of its inputs is read out of the pin: the compaction latch, which
    /// the turn that carried `x-claude-code-context-compacted` set and every
    /// later turn of the session reads back. It runs with the store lock
    /// released — the pin is copied out first — so scoring a long transcript
    /// never serializes other router-backed requests behind it.
    ///
    /// `read_only` is set for `count_tokens`, which must reach the same tier as
    /// the real turn without recording anything: Claude Code sends those probes
    /// with a history one turn behind, so a committing probe would let the probe
    /// drive the pin.
    ///
    /// `now` is injected so dwell, TTL, and eviction are deterministically
    /// testable.
    ///
    /// Returns the decision and, unless the turn is read-only or sessionless,
    /// the [`PendingPin`] it earned. Nothing is written until that pin is handed
    /// to [`StageRouterStore::commit`] — see [`PendingPin`] for why the write
    /// waits for the request to be admitted.
    pub(crate) fn apply(
        &self,
        model: &str,
        hints: &RouterContext<'_>,
        router: &StageRouterConfig,
        estimate: impl FnOnce(bool) -> StageDecision,
        read_only: bool,
        now: Instant,
    ) -> StageApplied {
        // An empty header is not a session. Without this every client that
        // sends a blank `x-claude-code-session-id` would hash to one key and
        // share a single tier pin for the model — the same reason the websocket
        // pool (`adapters/responses/mod.rs`) and the inbound Codex endpoint
        // already filter it before using the id as a sticky key.
        let Some(session_id) = hints.session_id.filter(|session_id| !session_id.is_empty()) else {
            // A sessionless request has no pin to latch onto, so the header
            // counts for this turn alone.
            return StageApplied::stateless(estimate(hints.context_compacted));
        };
        let key = session_key(model, session_id, hints.pin_agent_id());
        let fingerprint = fingerprint(router);
        // Taken before the lock, so the number reflects the order requests
        // arrived at this function rather than the order they won the mutex.
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let ttl = Duration::from_secs(router.session_ttl_seconds);

        // A pin from a different router table, or one that has gone quiet longer
        // than its TTL, is treated as absent.
        //
        // Expiry reads the entry's own TTL, as [`evict`] does. The caller's
        // would give the same answer — a table whose `session_ttl_seconds`
        // differs also hashes to a different `fingerprint`, which this filter
        // already rejects — but only by way of the hash, which is not something
        // these two lines show on their own.
        //
        // The lock is held for the lookup alone: `apply` only reads, the entry
        // is `Copy`, and the write that used to happen here now waits for
        // [`StageRouterStore::commit`].
        let pinned = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .copied()
            .filter(|session| is_live(session, fingerprint, now));

        // The latch: this turn's header, or what an earlier turn of the session
        // recorded. Read before scoring because libsy's compaction override is
        // an input to the estimate, not a filter on it.
        let compacted = hints.context_compacted || pinned.is_some_and(|session| session.compacted);
        let resolved = resolve(router, pinned, estimate(compacted));
        let (decision, changed) = (resolved.decision, resolved.changed);

        if read_only {
            // A probe changes nothing, so it displaced nothing: reporting a
            // previous tier here would let `count_tokens` count as a flip.
            return StageApplied::stateless(decision);
        }

        let dwell_turns = match (pinned, changed) {
            (Some(session), false) => session.dwell_turns.saturating_add(1),
            // A flip, or the first turn of a new session, restarts the window.
            // It starts at 1 because this turn is itself served at the new tier,
            // so `min_dwell_turns = 3` means three turns served before the tier
            // may move again.
            _ => 1,
        };
        // Only a router that can consult pays for the key's digest: every other
        // routed turn — the hot path the `stage_router` bench prices — has no
        // judge to count.
        let budget = router.classifier.as_ref().map(|_| StageBudget {
            key: JudgeBudget::stage_key(model, fingerprint, session_id, BudgetScope::of(hints)),
            ttl,
        });
        StageApplied {
            decision,
            judge_budget: budget,
            pin: Some(PendingPin {
                key,
                observed_compaction: hints.context_compacted,
                budget,
                session: StageSession {
                    seq,
                    tier: decision.tier,
                    fingerprint,
                    dwell_turns,
                    last_seen: now,
                    ttl,
                    compacted,
                    capable_hold_remaining: resolved.capable_hold_remaining,
                },
            }),
        }
    }

    /// Write a pin [`StageRouterStore::apply`] prepared, once the request that
    /// earned it has been admitted.
    ///
    /// The lock is taken again rather than held across admission: auth and the
    /// managed-model policy run in between, and holding a store-wide mutex
    /// across them would serialize every router-backed request behind the
    /// slowest one.
    ///
    /// Releasing it means two concurrent turns of one session can both decide
    /// against the same pin and then commit in either order — and the one that
    /// commits *last* is not necessarily the one that decided last. The write is
    /// therefore conditional: a pending pin is dropped when the entry already
    /// there came from a *later* decision (a higher `seq`). Without that check an older turn
    /// landing second would install its own tier and dwell count over a newer
    /// turn's, regressing the session to a decision made from staler history.
    ///
    /// The comparison is on `seq` alone, deliberately. Scoping it to a matching
    /// `fingerprint` — so a reconfigured table could replace an old-table pin —
    /// reads as the careful choice and is the wrong one: two requests straddling
    /// a hot reload hold different fingerprints, so the check would not apply and
    /// the *older* one would overwrite the newer table's pin, leaving an entry
    /// the next request rejects as stale. `seq` already covers the case the
    /// carve-out was for: a request decided under the new table necessarily has
    /// the higher `seq`, so it wins without needing an exemption.
    ///
    /// Returns the `(from, to)` tier change this write actually made, and
    /// `None` when it made none. That is deliberately decided *here* rather
    /// than from the snapshot [`StageRouterStore::apply`] read: the lock is
    /// released in between, so two concurrent turns of one session both read
    /// `efficient`, both decide `capable`, and both would report a flip for a
    /// session that moved once. Only one of them displaces an `efficient`
    /// entry — the other finds `capable` already there, or is superseded — so
    /// counting what the write did counts each move once.
    pub(crate) fn commit(&self, pin: PendingPin, now: Instant) -> Option<(StageTier, StageTier)> {
        // The turn was served, so its session is alive whether or not its pin
        // survives the `seq` race below. Refreshed first, under the budget's own
        // lock, so the two locks are never held together.
        if let Some(budget) = pin.budget {
            self.judge_budget.touch(&budget.key, budget.window(now));
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = entries.get(&pin.key).copied();
        // Filtered by the same predicate `apply` reads pins through: an entry
        // from another config generation, or one that has gone quiet past its
        // TTL, is absent to the next request and so displaces nothing. Without
        // the filter a resumed session would report a flip away from a tier no
        // request could have been served at.
        let live = current.filter(|current| is_live(current, pin.session.fingerprint, now));
        let superseded = current.is_some_and(|current| current.seq > pin.session.seq);
        if superseded {
            // The tier this turn decided is stale, but the compaction flag it
            // carried is not. `x-claude-code-context-compacted` is one-shot —
            // the client consumes it as it sends it — so discarding the whole
            // write would lose the escalation for the rest of the pin's life,
            // and no later turn could reconstruct it. The turn was served, so
            // keep just that flag and drop the tier and dwell it decided.
            //
            // Only onto a live entry: a pin from another config generation, or
            // one past its TTL, is exactly where the latch is meant to clear.
            //
            // And only for the turn that actually carried the header —
            // `session.compacted` may have been inherited from a pin that has
            // since expired, and re-latching that would defeat the TTL reset.
            if pin.observed_compaction && live.is_some() {
                entries.latch_compacted(&pin.key);
            }
            return None;
        }
        let previous = live.map(|current| current.tier);
        let mut session = pin.session;
        // The latch only ever goes false -> true within one live pin. This turn
        // decided against a snapshot read before the lock was released, so a
        // `false` here means "no header on *this* turn", never "the session is
        // no longer compacted"; without the merge an ordinary turn racing the
        // compacted one would clear a latch it never saw. The two documented
        // ways out — TTL expiry and a table reload — both make `live` `None`.
        session.compacted |= live.is_some_and(|live| live.compacted);
        let scope = PinScope::of(&pin.key);
        entries.insert(pin.key, session);
        evict(&mut entries, scope, now);
        previous
            .filter(|previous| *previous != session.tier)
            .map(|previous| (previous, session.tier))
    }

    /// Reserve one judge call against `max` for the turn that holds `budget`,
    /// or refuse it (ADR-0005 §3, issue #634).
    ///
    /// Called immediately before the judge is dispatched, and the reservation
    /// *is* the check: one lock acquisition reads the count and writes it, with
    /// no `.await` between, so `N` concurrent turns of a session racing for its
    /// last call admit one. It works for a session's very first turns as well,
    /// which have no pin yet, because the count does not need one — which is
    /// also why it publishes nothing a concurrent turn could read as a tier.
    /// Nothing refunds it: a turn whose pin later loses the `seq` race in
    /// [`StageRouterStore::commit`] still made its call.
    ///
    /// `None` — a sessionless turn — is always admitted: the stage router
    /// consults at most once per turn, so that is the turn's one call.
    pub(crate) fn try_charge_judge(
        &self,
        budget: Option<&StageBudget>,
        max: u32,
        now: Instant,
    ) -> bool {
        self.judge_budget.try_charge_within(
            budget.map(|budget| &budget.key),
            max,
            budget.map(|budget| budget.window(now)),
        )
    }

    /// [`StageRouterStore::apply`] with a session id in place of the full hint
    /// set and a ready estimate in place of the closure — the signature the
    /// store had before the hints existed. Tests that are not about the hints
    /// use this so they assert on the same end-to-end behaviour.
    #[cfg(test)]
    fn apply_session(
        &self,
        model: &str,
        session_id: Option<&str>,
        router: &StageRouterConfig,
        estimate: StageDecision,
        read_only: bool,
        now: Instant,
    ) -> StageApplied {
        self.apply(
            model,
            &RouterContext::session(session_id),
            router,
            |_| estimate,
            read_only,
            now,
        )
    }

    /// `apply` followed immediately by `commit`, the shape the store had before
    /// the write was deferred past admission. Tests that are not about the split
    /// itself use this so they assert on the same end-to-end behaviour.
    #[cfg(test)]
    fn apply_now(
        &self,
        model: &str,
        session_id: Option<&str>,
        router: &StageRouterConfig,
        estimate: StageDecision,
        read_only: bool,
        now: Instant,
    ) -> StageDecision {
        let applied = self.apply_session(model, session_id, router, estimate, read_only, now);
        if let Some(pin) = applied.pin {
            self.commit(pin, now);
        }
        applied.decision
    }

    /// Judge calls `budget`'s session holds at `now`, for the budget tests.
    #[cfg(test)]
    pub(crate) fn judge_calls_at(&self, budget: &StageBudget, now: Instant) -> u32 {
        self.judge_budget.used_at(&budget.key, now)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod tests;
