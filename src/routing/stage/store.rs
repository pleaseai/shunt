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
        let Some(session_id) = session_id else {
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
        let pinned = entries.get(&key).copied().filter(|session| {
            session.fingerprint == fingerprint
                && now.saturating_duration_since(session.last_seen) <= ttl
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
            },
        );
        evict(&mut entries, now, ttl);

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
fn evict(entries: &mut HashMap<SessionKey, StageSession>, now: Instant, ttl: Duration) {
    if entries.len() <= MAX_TRACKED_SESSIONS {
        return;
    }
    entries.retain(|_, session| now.saturating_duration_since(session.last_seen) <= ttl);
    while entries.len() > MAX_TRACKED_SESSIONS {
        let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, session)| session.last_seen)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        entries.remove(&oldest);
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

/// Hysteresis tests.
///
/// These pin the asymmetry and the invalidation rules, not libsy's scoring —
/// every estimate here is constructed by hand so a test states exactly the
/// scorer output it is reasoning about. Non-vacuity: drop the `Capable` arm's
/// dwell/threshold gate and `a_pinned_capable_tier_holds_through_a_weak_estimate`
/// plus `a_de_escalation_also_needs_the_dwell_window` go red; drop the
/// `Efficient` arm's immediate flip and `an_escalation_needs_no_dwell` goes red;
/// stop recording in `read_only` mode and `a_count_tokens_probe_never_moves_the_pin`
/// goes red; make the fingerprint constant and
/// `a_reconfigured_router_abandons_its_pins` goes red; drop the `tests_passed`
/// exemption from the confidence gate and
/// `a_passing_test_suite_de_escalates_without_a_confidence_score` goes red.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StageRouterPicker;

    const SESSION: &str = "0199a0f2-2f4b-7c3e-9d61-4f1a2b3c4d5e";

    fn router() -> StageRouterConfig {
        StageRouterConfig {
            capable_target: "claude-opus-4-8".to_string(),
            efficient_target: "claude-sonnet-4-6".to_string(),
            picker: StageRouterPicker::EfficientFirst,
            confidence_threshold: crate::config::DEFAULT_CONFIDENCE_THRESHOLD,
            recent_turn_window: 3,
            min_dwell_turns: 3,
            deescalate_threshold: None,
            session_ttl_seconds: 3600,
        }
    }

    fn decision(tier: StageTier, source: &'static str, confidence: f64) -> StageDecision {
        StageDecision {
            tier,
            source,
            confidence: Some(confidence),
        }
    }

    fn capable() -> StageDecision {
        decision(StageTier::Capable, "dimensions", 0.76)
    }

    fn efficient(confidence: f64) -> StageDecision {
        decision(StageTier::Efficient, "dimensions", confidence)
    }

    /// Serve `turns` turns at whatever the estimate says, so a pin accrues dwell.
    fn pin(
        store: &StageRouterStore,
        router: &StageRouterConfig,
        estimate: StageDecision,
        turns: u32,
    ) {
        let now = Instant::now();
        for _ in 0..turns {
            store.apply("claude-auto", Some(SESSION), router, estimate, false, now);
        }
    }

    #[test]
    fn a_request_without_a_session_id_is_decided_statelessly() {
        // A bare `curl` sends no session header. It must still route, and must
        // not take a slot that a real session could use.
        let store = StageRouterStore::new();
        let router = router();

        let decision = store.apply(
            "claude-auto",
            None,
            &router,
            capable(),
            false,
            Instant::now(),
        );

        assert_eq!(decision.tier, StageTier::Capable);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn a_count_tokens_probe_never_moves_the_pin() {
        // Claude Code sends count_tokens with a history one turn behind. A probe
        // that committed would let the stale history drive the pin.
        let store = StageRouterStore::new();
        let router = router();
        let now = Instant::now();

        let probe = store.apply("claude-auto", Some(SESSION), &router, capable(), true, now);

        assert_eq!(probe.tier, StageTier::Capable, "a probe still gets a tier");
        assert_eq!(store.len(), 0, "but it leaves no trace");
    }

    /// The asymmetry, upward half: a turn the efficient tier cannot serve costs
    /// more than the cache prefix an immediate flip forfeits.
    #[test]
    fn an_escalation_needs_no_dwell() {
        let store = StageRouterStore::new();
        let router = router();
        let now = Instant::now();

        store.apply(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.9),
            false,
            now,
        );
        let escalated = store.apply("claude-auto", Some(SESSION), &router, capable(), false, now);

        assert_eq!(escalated.tier, StageTier::Capable);
        assert_eq!(escalated.source, "dimensions");
    }

    /// The asymmetry, downward half: 0.6 clears the escalate threshold (0.5) but
    /// not the de-escalate one (0.75), so the pin holds.
    #[test]
    fn a_pinned_capable_tier_holds_through_a_weak_estimate() {
        let store = StageRouterStore::new();
        let router = router();
        assert_eq!(
            router.deescalate_threshold(),
            crate::config::DEFAULT_DEESCALATE_THRESHOLD
        );
        pin(&store, &router, capable(), 5);

        let held = store.apply(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.6),
            false,
            Instant::now(),
        );

        assert_eq!(held.tier, StageTier::Capable);
        assert_eq!(held.source, "sticky");
    }

    #[test]
    fn a_confident_estimate_de_escalates_once_the_dwell_window_has_passed() {
        let store = StageRouterStore::new();
        let router = router();
        pin(&store, &router, capable(), 5);

        let dropped = store.apply(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.8),
            false,
            Instant::now(),
        );

        assert_eq!(dropped.tier, StageTier::Efficient);
        assert_eq!(dropped.source, "dimensions");
    }

    /// Confidence alone is not enough: the tier must also have been held long
    /// enough that the forfeited prompt cache was worth building.
    #[test]
    fn a_de_escalation_also_needs_the_dwell_window() {
        let store = StageRouterStore::new();
        let router = router();
        pin(&store, &router, capable(), 1);

        let held = store.apply(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.9),
            false,
            Instant::now(),
        );

        assert_eq!(
            held.tier,
            StageTier::Capable,
            "one turn is not a dwell window"
        );
        assert_eq!(held.source, "sticky");
    }

    /// libsy's hard de-escalation shortcut skips the scorer and reports no
    /// confidence at all (`resolved(Efficient, TestsPassed, 0.0, None)`), so a
    /// bare `confidence >= threshold` gate would make the single strongest
    /// reason to go cheap the one reason that can never fire.
    #[test]
    fn a_passing_test_suite_de_escalates_without_a_confidence_score() {
        let store = StageRouterStore::new();
        let router = router();
        pin(&store, &router, capable(), 5);

        let dropped = store.apply(
            "claude-auto",
            Some(SESSION),
            &router,
            StageDecision {
                tier: StageTier::Efficient,
                source: "tests_passed",
                confidence: None,
            },
            false,
            Instant::now(),
        );

        assert_eq!(dropped.tier, StageTier::Efficient);
        assert_eq!(dropped.source, "tests_passed");
    }

    /// It still waits out the dwell window: that gate prices the forfeited
    /// prompt cache, which costs the same however strong the evidence is.
    #[test]
    fn even_a_passing_test_suite_waits_out_the_dwell_window() {
        let store = StageRouterStore::new();
        let router = router();
        pin(&store, &router, capable(), 1);

        let held = store.apply(
            "claude-auto",
            Some(SESSION),
            &router,
            StageDecision {
                tier: StageTier::Efficient,
                source: "tests_passed",
                confidence: None,
            },
            false,
            Instant::now(),
        );

        assert_eq!(held.tier, StageTier::Capable);
        assert_eq!(held.source, "sticky");
    }

    /// A fall-open is the picker's default, not evidence. It must not be able to
    /// unpin a tier the signals chose — in either direction.
    #[test]
    fn a_fall_open_estimate_cannot_move_a_pinned_tier() {
        let store = StageRouterStore::new();
        let router = router();
        pin(&store, &router, capable(), 5);

        for source in ["fall_open", "no_signal", "ambiguous"] {
            let held = store.apply(
                "claude-auto",
                Some(SESSION),
                &router,
                decision(StageTier::Efficient, source, 0.9),
                false,
                Instant::now(),
            );
            assert_eq!(held.tier, StageTier::Capable, "{source} must not unpin");
            assert_eq!(held.source, "sticky");
        }
    }

    #[test]
    fn a_reconfigured_router_abandons_its_pins() {
        let store = StageRouterStore::new();
        let router = router();
        pin(&store, &router, capable(), 5);

        let mut reloaded = router.clone();
        reloaded.efficient_target = "claude-haiku-4-5".to_string();
        let decision = store.apply(
            "claude-auto",
            Some(SESSION),
            &reloaded,
            efficient(0.6),
            false,
            Instant::now(),
        );

        assert_eq!(
            decision.tier,
            StageTier::Efficient,
            "a pin made under a different table must not bind the new one"
        );
    }

    #[test]
    fn a_pin_expires_once_the_session_goes_quiet() {
        let store = StageRouterStore::new();
        let mut router = router();
        router.session_ttl_seconds = 60;
        let start = Instant::now();
        store.apply(
            "claude-auto",
            Some(SESSION),
            &router,
            capable(),
            false,
            start,
        );

        let later = start + Duration::from_secs(61);
        let decision = store.apply(
            "claude-auto",
            Some(SESSION),
            &router,
            efficient(0.6),
            false,
            later,
        );

        assert_eq!(
            decision.tier,
            StageTier::Efficient,
            "an expired pin must not bind, even below the de-escalate threshold"
        );
    }

    #[test]
    fn two_router_models_in_one_session_keep_independent_tiers() {
        let store = StageRouterStore::new();
        let router = router();
        let now = Instant::now();

        store.apply("claude-auto", Some(SESSION), &router, capable(), false, now);
        let other = store.apply(
            "claude-cheap",
            Some(SESSION),
            &router,
            efficient(0.9),
            false,
            now,
        );

        assert_eq!(other.tier, StageTier::Efficient);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn the_store_evicts_the_oldest_session_once_it_is_full() {
        let store = StageRouterStore::new();
        let router = router();
        let start = Instant::now();

        // The oldest session is inserted first and never touched again.
        store.apply(
            "claude-auto",
            Some(SESSION),
            &router,
            capable(),
            false,
            start,
        );
        for index in 0..MAX_TRACKED_SESSIONS {
            let session = format!("session-{index}");
            // Milliseconds apart, so every filler stays well inside the TTL and
            // the cap — not expiry — is what does the evicting here.
            let now = start + Duration::from_millis(index as u64 + 1);
            store.apply(
                "claude-auto",
                Some(&session),
                &router,
                capable(),
                false,
                now,
            );
        }

        assert_eq!(store.len(), MAX_TRACKED_SESSIONS);
        let key = session_key("claude-auto", SESSION);
        assert!(
            !store
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&key),
            "the least recently seen session is the one dropped"
        );
    }
}
