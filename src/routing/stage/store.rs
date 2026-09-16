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

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use super::{StageDecision, StageSource, StageTier};
use crate::config::StageRouterConfig;

/// Upper bound on tracked sessions. Each entry is well under 100 bytes, so the
/// whole store stays in the low hundreds of kilobytes.
pub(crate) const MAX_TRACKED_SESSIONS: usize = 4096;

/// `(advertised model id, SHA-256 prefix of the session id)`.
///
/// Two router-backed models used inside one Claude Code session keep
/// independent tiers. The session id is hashed and never stored, matching how
/// `accounts::stable_session_index` treats it.
type SessionKey = (String, [u8; 16]);

#[derive(Debug, Clone, Copy)]
struct StageSession {
    /// Which decision this entry came from, in the order `apply` made them.
    ///
    /// The ordering [`StageRouterStore::commit`] needs, and `last_seen` cannot
    /// supply: two turns of one session routinely share an `Instant` — the work
    /// between them can be finer than the clock's resolution, and a test injects
    /// one `now` for several turns — so a timestamp comparison cannot tell
    /// "already superseded" from "decided in the same tick", and would drop the
    /// second turn's write.
    seq: u64,
    tier: StageTier,
    /// Hash of the router table in force when this tier was chosen. A config
    /// reload that changes the table invalidates this entry alone, so an
    /// unrelated edit elsewhere in the config does not drop every live pin.
    fingerprint: u64,
    /// Turns served at this tier, counting the one that chose it.
    dwell_turns: u32,
    last_seen: Instant,
    /// The `session_ttl_seconds` of the router that wrote this entry.
    ///
    /// Carried per entry because the store is shared across every
    /// router-backed model, and [`evict`] runs over all of them on whichever
    /// request happened to overflow the cap. Expiring with the *caller's* TTL
    /// would let a model configured with a one-second window delete another
    /// model's seconds-old pin out from under a one-hour window — and an
    /// unpinned session is free to de-escalate immediately, which is the exact
    /// flip the dwell gate exists to prevent.
    ttl: Duration,
}

/// A pin [`StageRouterStore::apply`] prepared but has not written.
///
/// Deciding and recording are separate because routing runs *before* inbound
/// auth and the managed-model policy — `check_inbound_auth` needs the resolved
/// chain, so it cannot run first. Committing inside `apply` therefore let a
/// request that was about to be rejected create or evict pins, and let a caller
/// who guessed another client's session id steer that client's next turn
/// without ever presenting a credential. The decision still happens under one
/// lock with the pin it read; only the write-back is deferred, to the point
/// where the request is known to be served.
#[derive(Debug)]
pub(crate) struct PendingPin {
    key: SessionKey,
    session: StageSession,
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
    entries: Mutex<HashMap<SessionKey, StageSession>>,
    /// Hands out the `seq` above. Monotonic for the process, so it orders
    /// decisions across every session and router without a per-key counter.
    next_seq: AtomicU64,
}

impl StageRouterStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Apply hysteresis to one turn's `estimate` and return the tier that serves it.
    ///
    /// `session_id` is `None` for a caller that sends no
    /// `x-claude-code-session-id` (Claude Code always does; a bare `curl` does
    /// not). Such a request is scored statelessly and never touches the store.
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
        session_id: Option<&str>,
        router: &StageRouterConfig,
        estimate: StageDecision,
        read_only: bool,
        now: Instant,
    ) -> StageApplied {
        // An empty header is not a session. Without this every client that
        // sends a blank `x-claude-code-session-id` would hash to one key and
        // share a single tier pin for the model — the same reason the websocket
        // pool (`adapters/responses/mod.rs`) and the inbound Codex endpoint
        // already filter it before using the id as a sticky key.
        let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) else {
            return StageApplied::stateless(estimate);
        };
        let key = session_key(model, session_id);
        let fingerprint = fingerprint(router);
        // Taken before the lock, so the number reflects the order requests
        // arrived at this function rather than the order they won the mutex.
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let ttl = Duration::from_secs(router.session_ttl_seconds);

        // A shared borrow: `apply` only reads. The write that used to happen
        // here now waits for [`StageRouterStore::commit`].
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // A pin from a different router table, or one that has gone quiet longer
        // than its TTL, is treated as absent.
        //
        // Expiry reads the entry's own TTL, as [`evict`] does. The caller's
        // would give the same answer — a table whose `session_ttl_seconds`
        // differs also hashes to a different `fingerprint`, which this filter
        // already rejects — but only by way of the hash, which is not something
        // these two lines show on their own.
        let pinned = entries
            .get(&key)
            .copied()
            .filter(|session| is_live(session, fingerprint, now));

        let (decision, changed) = resolve(router, pinned, estimate);

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
                session: StageSession {
                    seq,
                    tier: decision.tier,
                    fingerprint,
                    dwell_turns,
                    last_seen: now,
                    ttl,
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
        let superseded = current.is_some_and(|current| current.seq > pin.session.seq);
        if superseded {
            return None;
        }
        // Filtered by the same predicate `apply` reads pins through: an entry
        // from another config generation, or one that has gone quiet past its
        // TTL, is absent to the next request and so displaces nothing. Without
        // the filter a resumed session would report a flip away from a tier no
        // request could have been served at.
        let previous = current
            .filter(|current| is_live(current, pin.session.fingerprint, now))
            .map(|current| current.tier);
        entries.insert(pin.key, pin.session);
        evict(&mut entries, now);
        previous
            .filter(|previous| *previous != pin.session.tier)
            .map(|previous| (previous, pin.session.tier))
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
        let applied = self.apply(model, session_id, router, estimate, read_only, now);
        if let Some(pin) = applied.pin {
            self.commit(pin, now);
        }
        applied.decision
    }

    #[cfg(test)]
    fn len(&self) -> usize {
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
            // `tests_passed` is libsy's hard de-escalation shortcut: it skips
            // the scorer outright and so reports no confidence at all. Holding
            // it to a confidence floor would make the strongest reason to go
            // cheap the one reason that can never fire. The dwell window still
            // applies — that gate prices the forfeited prompt cache, which
            // costs the same however good the evidence is.
            let convincing = estimate.source.is_tests_passed()
                || estimate
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

/// Drop expired entries, then the oldest, until the store is back under its cap.
/// Amortised onto the insert path: no timer, no background task.
///
/// Each entry is expired against **its own** TTL, not the caller's — see
/// [`StageSession::ttl`]. One pass does both jobs: the retain predicate drops
/// what has expired and remembers the oldest survivor, so the capacity trim
/// below needs no second scan of the map while the lock is held.
fn evict(entries: &mut HashMap<SessionKey, StageSession>, now: Instant) {
    if entries.len() <= MAX_TRACKED_SESSIONS {
        return;
    }
    let mut oldest: Option<(SessionKey, Instant)> = None;
    entries.retain(|key, session| {
        if now.saturating_duration_since(session.last_seen) > session.ttl {
            return false;
        }
        if oldest
            .as_ref()
            .is_none_or(|(_, last_seen)| session.last_seen < *last_seen)
        {
            oldest = Some((key.clone(), session.last_seen));
        }
        true
    });
    // `apply` inserts exactly one entry before calling this and returns early
    // above while under the cap, so the store is at most one over it here and
    // a single removal is enough. The loop is still a loop so that a future
    // caller inserting in bulk cannot silently leave the cap exceeded.
    while entries.len() > MAX_TRACKED_SESSIONS {
        let Some(key) = oldest.take().map(|(key, _)| key).or_else(|| {
            entries
                .iter()
                .min_by_key(|(_, session)| session.last_seen)
                .map(|(key, _)| key.clone())
        }) else {
            break;
        };
        entries.remove(&key);
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

fn session_key(model: &str, session_id: &str) -> SessionKey {
    let digest = Sha256::digest(session_id.as_bytes());
    let prefix = digest[..16].try_into().expect("SHA-256 prefix is 16 bytes");
    (model.to_string(), prefix)
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
mod tests;
