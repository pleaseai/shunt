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

mod entries;

use std::{
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use super::{StageDecision, StageSource, StageTier};
use crate::config::StageRouterConfig;
use crate::routing::context::RouterContext;
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
}

/// What one [`StageRouterStore::apply`] call decided, and what it displaced.
pub(crate) struct StageApplied {
    pub decision: StageDecision,
    /// The pin this turn earned, parked until the request is admitted. `None`
    /// for a turn that records nothing: no session id, or a read-only probe.
    pub pin: Option<PendingPin>,
}

impl StageApplied {
    /// A decision that records no pin, and so can displace none.
    fn stateless(decision: StageDecision) -> Self {
        Self {
            decision,
            pin: None,
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
}

impl StageRouterStore {
    pub(crate) fn new() -> Self {
        Self::default()
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
        let (decision, changed) = resolve(router, pinned, estimate(compacted));

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
        StageApplied {
            decision,
            pin: Some(PendingPin {
                key,
                observed_compaction: hints.context_compacted,
                session: StageSession {
                    seq,
                    tier: decision.tier,
                    fingerprint,
                    dwell_turns,
                    last_seen: now,
                    ttl,
                    compacted,
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

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// Decide this turn's tier from the pin and the estimate. Returns the decision
/// and whether it differs from what was pinned.
fn resolve(
    router: &StageRouterConfig,
    pinned: Option<StageSession>,
    estimate: StageDecision,
) -> (StageDecision, bool) {
    let Some(session) = pinned else {
        return (estimate, true);
    };
    if session.tier == estimate.tier {
        return (estimate, false);
    }

    let held = StageDecision {
        tier: session.tier,
        source: StageSource::Sticky,
        confidence: estimate.confidence,
    };

    // Only a decision the signals actually made may move a pinned tier. A
    // fall-open or a signal-less turn is the picker's default, not evidence.
    if !estimate.source.is_signal_evidence() {
        return (held, false);
    }

    match session.tier {
        // Up is the cheap direction: a turn the efficient tier cannot serve
        // costs more than the forfeited cache prefix. No dwell requirement.
        StageTier::Efficient => (estimate, true),
        // Down is the expensive direction, and must clear both gates.
        StageTier::Capable => {
            // Every source `is_signal_evidence` admits now reports a
            // confidence, so the floor applies to all of them. libsy's one
            // confidence-less de-escalation — the `tests_passed` shortcut,
            // which used to need an exemption here — no longer exists: upstream
            // dropped it, and a passing test now only clears libsy's own
            // capable hold. The dwell window still applies on top; that gate
            // prices the forfeited prompt cache, which costs the same however
            // good the evidence is.
            let convincing = estimate
                .confidence
                .is_some_and(|confidence| confidence >= router.deescalate_threshold());
            if session.dwell_turns >= router.min_dwell_turns && convincing {
                (estimate, true)
            } else {
                (held, false)
            }
        }
    }
}

/// Drop expired entries, then the oldest of `scope`, until that scope is back
/// under its cap. Amortised onto the insert path: no timer, no background task.
///
/// Every step is one `BTreeMap` pop, so this costs O(log n) per entry removed
/// rather than a walk of the whole map. That is issue #552: the map is read
/// under the store's single process-global mutex, so a full 4096-entry scan on
/// every previously-unseen session id — the steady state for a client that
/// rotates `x-claude-code-session-id` — serialized against every other
/// router-backed request, not only its own.
///
/// Each entry is expired against **its own** TTL, not the caller's — see
/// [`StageSession::ttl`].
///
/// Only the scope the insert grew is trimmed, and only against its own
/// budget: a full child budget pops the oldest *child*, never a parent that is
/// idle because it is waiting on those children (ADR-0005 §5). The expiry
/// sweep is scope-blind, as expiry is.
///
/// Expiry and recency are indexed separately, so this drops the same entries the
/// whole-map pass dropped: *every* expired one, and then the oldest survivor if
/// the scope is still over. Sweeping the front of the recency order alone would
/// not — with two routers configuring different `session_ttl_seconds` the oldest
/// entry can be the live one, which would stop the sweep and then be evicted in
/// place of the expired entry behind it.
fn evict(entries: &mut Entries, scope: PinScope, now: Instant) {
    let cap = scope.cap();
    if entries.len_of(scope) <= cap {
        return;
    }
    entries.drain_expired(now);
    // `commit` inserts exactly one entry before calling this and returns early
    // above while under the cap, so the scope is at most one over it here and
    // a single removal is enough. The loop is still a loop so that a future
    // caller inserting in bulk cannot silently leave the cap exceeded.
    while entries.len_of(scope) > cap {
        if entries.remove_oldest(scope).is_none() {
            break;
        }
    }
}

/// Whether a stored entry still speaks for the session, for the one router
/// table identified by `fingerprint`.
///
/// Read by `apply` before it treats an entry as a pin and by `commit` before it
/// treats one as displaced, because those two must agree: a pin `apply` ignored
/// as stale must not surface as the `from` side of a flip.
fn is_live(session: &StageSession, fingerprint: u64, now: Instant) -> bool {
    session.fingerprint == fingerprint
        && now.saturating_duration_since(session.last_seen) <= session.ttl
}

/// Hash the router table a pinned decision was made under.
///
/// `f64` is hashed through `to_bits` because it is not `Hash`, and the
/// *effective* de-escalation threshold is hashed rather than the `Option` so
/// that omitting the key and writing its default are the same table. Comparisons
/// only ever happen inside one process against one other fingerprint, so
/// `DefaultHasher` not being stable across Rust versions does not matter — the
/// store is never persisted.
fn fingerprint(router: &StageRouterConfig) -> u64 {
    // Destructured rather than dotted, so a key added to the table later fails
    // to compile here instead of silently letting stale pins outlive it.
    let StageRouterConfig {
        capable_target,
        efficient_target,
        picker,
        confidence_threshold,
        recent_turn_window,
        min_dwell_turns,
        deescalate_threshold: _,
        session_ttl_seconds,
    } = router;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    capable_target.hash(&mut hasher);
    efficient_target.hash(&mut hasher);
    picker.hash(&mut hasher);
    confidence_threshold.to_bits().hash(&mut hasher);
    router.deescalate_threshold().to_bits().hash(&mut hasher);
    recent_turn_window.hash(&mut hasher);
    min_dwell_turns.hash(&mut hasher);
    session_ttl_seconds.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod tests;
