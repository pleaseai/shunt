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
    sync::Mutex,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use super::{StageDecision, StageTier};
use crate::config::StageRouterConfig;

/// Upper bound on tracked sessions. Each entry is well under 100 bytes, so the
/// whole store stays in the low hundreds of kilobytes.
const MAX_TRACKED_SESSIONS: usize = 4096;

/// `(advertised model id, SHA-256 prefix of the session id)`.
///
/// Two router-backed models used inside one Claude Code session keep
/// independent tiers. The session id is hashed and never stored, matching how
/// `accounts::stable_session_index` treats it.
type SessionKey = (String, [u8; 16]);

#[derive(Debug, Clone, Copy)]
struct StageSession {
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

/// Per-session tier pins. Lives on `AppState` beside the account pool, so it
/// survives a config reload rather than being rebuilt by one.
#[derive(Debug, Default)]
pub(crate) struct StageRouterStore {
    entries: Mutex<HashMap<SessionKey, StageSession>>,
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
    pub(crate) fn apply(
        &self,
        model: &str,
        session_id: Option<&str>,
        router: &StageRouterConfig,
        estimate: StageDecision,
        read_only: bool,
        now: Instant,
    ) -> StageDecision {
        // An empty header is not a session. Without this every client that
        // sends a blank `x-claude-code-session-id` would hash to one key and
        // share a single tier pin for the model — the same reason the websocket
        // pool (`adapters/responses/mod.rs`) and the inbound Codex endpoint
        // already filter it before using the id as a sticky key.
        let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) else {
            return estimate;
        };
        let key = session_key(model, session_id);
        let fingerprint = fingerprint(router);
        let ttl = Duration::from_secs(router.session_ttl_seconds);

        let mut entries = self
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
        let pinned = entries.get(&key).copied().filter(|session| {
            session.fingerprint == fingerprint
                && now.saturating_duration_since(session.last_seen) <= session.ttl
        });

        let (decision, changed) = resolve(router, pinned, estimate);

        if read_only {
            return decision;
        }

        let dwell_turns = match (pinned, changed) {
            (Some(session), false) => session.dwell_turns.saturating_add(1),
            // A flip, or the first turn of a new session, restarts the window.
            // It starts at 1 because this turn is itself served at the new tier,
            // so `min_dwell_turns = 3` means three turns served before the tier
            // may move again.
            _ => 1,
        };
        entries.insert(
            key,
            StageSession {
                tier: decision.tier,
                fingerprint,
                dwell_turns,
                last_seen: now,
                ttl,
            },
        );
        evict(&mut entries, now);

        decision
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
        source: "sticky",
        confidence: estimate.confidence,
    };

    // Only a decision the signals actually made may move a pinned tier. A
    // fall-open or a signal-less turn is the picker's default, not evidence.
    if !decided_by_signals(estimate.source) {
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
            let convincing = estimate.source == "tests_passed"
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

/// Whether a decision source represents evidence rather than a default.
fn decided_by_signals(source: &str) -> bool {
    matches!(source, "dimensions" | "override" | "tests_passed")
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
