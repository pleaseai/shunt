//! The pure half of the pin store: what one turn decides, and what makes a
//! stored pin still speak for its session.
//!
//! Split out of [`super`] so the store module stays the concurrency surface —
//! the mutex, the two maps, the commit ordering — and this one stays a set of
//! free functions over values, which is what lets the store tests drive the
//! decision gates without building a store at all.

use std::hash::{Hash, Hasher};
use std::time::Instant;

use switchyard_libsy::DecisionSource;

use super::entries::{Entries, PinScope, StageSession};
use super::{StageDecision, StageSource, StageTier};
use crate::config::StageRouterConfig;

/// What [`resolve`] decided for one turn.
pub(super) struct Resolved {
    pub(super) decision: StageDecision,
    /// Whether the tier differs from what was pinned, which restarts dwell.
    pub(super) changed: bool,
    /// The capable-hold counter this turn's pin should carry.
    pub(super) capable_hold_remaining: u32,
}

/// Decide this turn's tier from the pin and the estimate.
///
/// Three gates, in the order a turn meets them: libsy's `capable_hold_turns`
/// window, shunt's evidence test, then shunt's dwell floor and de-escalation
/// threshold. The hold is checked first because it is unconditional — that is
/// what "held" means — and because a turn it covers must not consume the dwell
/// counter twice.
///
/// libsy's own early-clear for the hold never fires here: it clears on
/// `tests_passed`, which `signals::extract` deliberately never sets (Claude
/// Code runs tests through `Bash`, so deciding it would mean reading result
/// text). So the window always runs its full length.
pub(super) fn resolve(
    router: &StageRouterConfig,
    pinned: Option<StageSession>,
    estimate: StageDecision,
) -> Resolved {
    let Some(session) = pinned else {
        return Resolved {
            decision: estimate,
            changed: true,
            // A first turn that escalates on evidence opens the window; one
            // that lands capable by default does not — the hold prices a
            // *signal-driven* escalation, and a picker default is not one.
            capable_hold_remaining: opened_hold(router, estimate),
        };
    };
    // The hold, before anything else. While it is open the pin stays capable
    // however convincing the estimate is, and the counter spends one turn.
    if session.tier == StageTier::Capable && session.capable_hold_remaining > 0 {
        return Resolved {
            decision: StageDecision {
                tier: StageTier::Capable,
                source: StageSource::Scorer(DecisionSource::CapableHold),
                confidence: estimate.confidence,
            },
            changed: false,
            capable_hold_remaining: session.capable_hold_remaining - 1,
        };
    }
    if session.tier == estimate.tier {
        return Resolved {
            decision: estimate,
            changed: false,
            // Re-arm, so a session that keeps earning the capable tier gets a
            // fresh window instead of one that never comes back.
            //
            // This is reached only once the previous window is *spent*: an open
            // hold is caught by the branch above, which stamps `CapableHold` and
            // decrements. So the window is exactly `capable_hold_turns` long
            // from the escalation — a re-confirming signal inside it does not
            // extend it — and a signal that re-earns the tier on the turn after
            // it closes opens a full one again. That bound is the point: a hold
            // that any confirming turn could extend would never close for a
            // session that keeps scoring capable, which is a latch rather than
            // the price of one escalation.
            capable_hold_remaining: opened_hold(router, estimate),
        };
    }

    let held = StageDecision {
        tier: session.tier,
        source: StageSource::Sticky,
        confidence: estimate.confidence,
    };
    let stay = Resolved {
        decision: held,
        changed: false,
        capable_hold_remaining: 0,
    };

    // Only a decision the signals actually made may move a pinned tier. A
    // fall-open or a signal-less turn is the picker's default, not evidence.
    if !estimate.source.is_signal_evidence() {
        return stay;
    }

    match session.tier {
        // Up is the cheap direction: a turn the efficient tier cannot serve
        // costs more than the forfeited cache prefix. No dwell requirement.
        StageTier::Efficient => Resolved {
            decision: estimate,
            changed: true,
            capable_hold_remaining: opened_hold(router, estimate),
        },
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
                Resolved {
                    decision: estimate,
                    changed: true,
                    capable_hold_remaining: 0,
                }
            } else {
                stay
            }
        }
    }
}

/// The hold window a signal-driven move to the capable tier opens.
///
/// `0` for everything else — a de-escalation, a picker default, a fall-open —
/// and `0` for every deployment that leaves `capable_hold_turns` at its shunt
/// default, which is what makes this whole path invisible by default.
pub(super) fn opened_hold(router: &StageRouterConfig, estimate: StageDecision) -> u32 {
    if estimate.tier == StageTier::Capable && estimate.source.is_signal_evidence() {
        router.capable_hold_turns
    } else {
        0
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
pub(super) fn evict(entries: &mut Entries, scope: PinScope, now: Instant) {
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
pub(super) fn is_live(session: &StageSession, fingerprint: u64, now: Instant) -> bool {
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
pub(super) fn fingerprint(router: &StageRouterConfig) -> u64 {
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
        capable_hold_turns,
        tool_semantics,
        handoff_notes,
        classifier,
        judge_timeout_ms,
        judge_max_response_bytes,
        gated_max_bytes,
        gated_idle_ms,
        gated_max_duration_ms,
        max_judge_calls,
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
    capable_hold_turns.hash(&mut hasher);
    tool_semantics.hash(&mut hasher);
    handoff_notes.hash(&mut hasher);
    // The judge's own inputs. A pin carries a judge budget, so a table that
    // adds, removes, or re-points a classifier — or moves a bound the judge
    // runs under — must not inherit a count spent under the old one. `f64`
    // again through `to_bits`, and the six bounds are plain integers.
    classifier.as_ref().map(|c| &c.target).hash(&mut hasher);
    classifier
        .as_ref()
        .map(|c| c.base_threshold.to_bits())
        .hash(&mut hasher);
    judge_timeout_ms.hash(&mut hasher);
    judge_max_response_bytes.hash(&mut hasher);
    gated_max_bytes.hash(&mut hasher);
    gated_idle_ms.hash(&mut hasher);
    gated_max_duration_ms.hash(&mut hasher);
    max_judge_calls.hash(&mut hasher);
    hasher.finish()
}
