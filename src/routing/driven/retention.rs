//! What a driven entry has retained each session to, as far as shunt can see
//! it — a read-only shadow a `count_tokens` probe resolves against (ADR-0005
//! §3, issue #647).
//!
//! **Why a shadow, and not the algorithm.** A probe "never enters `drive`,
//! makes zero judge calls, resolves to the session's current pin or, when no
//! live pin exists, the algorithm's no-model-call decision". libsy keeps the
//! pin a probe needs — the affinity map (`util/affinity.rs`), a composite's
//! retained tier (`algorithms/composite.rs`), the escalation latch
//! (`algorithms/escalation.rs`) — private, and *every completed drive writes
//! it*: `FallThrough` emits `Event::Decision` on each outcome, which the
//! affinity processor latches, a fail-open default included. So a probe cannot
//! run the algorithm to ask, even with its model calls refused; the asking
//! would itself be the write the probe must not make. Instead each entry keeps
//! this record of what its real turns observably left behind: written only by
//! a real (admitted, non-probe) drive, read only by a probe.
//!
//! **Only what is observable is recorded.** The record is derived from each
//! drive's outcome — the served target and the evidence behind it — so it can
//! only ever hold a target libsy was seen to retain. Where an outcome does not
//! say what libsy retained, the record is left as it was rather than guessed
//! at ([`Policy`] spells each form's rule), and a probe on a session with no
//! record takes the no-model-call decision: the entry's fail-open target, which
//! is what `routing::resolve_chain` resolved it to provisionally.
//!
//! **Sessionless requests are not recorded.** libsy can key affinity on a
//! message hash (`message_hash_fallback`) when no session id is sent; shunt
//! deliberately does not mirror that. A probe's history runs one turn behind
//! the turn it measures, so its hash would not match the turn's anyway, and a
//! map keyed by a hash of the caller's transcript is a memory surface with no
//! session to bound it. A sessionless probe takes the no-model-call decision.
//!
//! **Keyed by digest, bounded per class, never cleared** — the same rules as
//! [`JudgeBudget`], for the same reasons: the caller's ids are hashed, never
//! held, and parents and delegated agents are each capped at [`HARD_CAP`]
//! keys, the least recently seen of an overflowing class evicted first. An
//! evicted session's probe falls back to the no-model-call decision; its real
//! turns are unaffected, because nothing on the request path reads this.
//!
//! **A reload forgets it**, exactly as it forgets the algorithm instance whose
//! state it mirrors: both live on the [`DrivenEntry`](super::DrivenEntry).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use switchyard_libsy::DecisionSource;

use super::budget::{BudgetKey, BudgetScope, JudgeBudget};
use super::drive::DrivenDecision;
use crate::routing::context::RouterContext;
use crate::routing::outcome::RouteSource;
use crate::routing::stage::StageSource;

/// Most keys one record holds per class — parent threads, delegated agents —
/// before the class's least recently seen entry is evicted. The cap
/// [`JudgeBudget`] uses.
const HARD_CAP: usize = 4_096;

/// How long an escalation latch survives an idle session.
///
/// Upstream's `SESSION_STATE_TTL` (`algorithms/fall_through.rs`): libsy drops a
/// session's `FallThrough` state — the latch with it — once it has gone this
/// long unaccessed. Its sweep runs on an interval, so libsy may hold a latch a
/// little past the hour; expiring here at the hour errs toward the
/// no-model-call decision, never toward a latch libsy has already dropped for
/// long.
const LATCH_IDLE_TTL: Duration = Duration::from_secs(60 * 60);

/// The target a probe on this session resolves to, and the source it reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Retained {
    pub target: String,
    pub source: RouteSource,
}

/// Which of libsy's retained states an entry's outcomes reveal, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Policy {
    /// Nothing a probe could read: `classify_trigger = "every_request"` on a
    /// classifier or overlay judges every turn afresh, and `advisor` reviews a
    /// turn it has already answered. A probe always takes the no-model-call
    /// decision.
    None,
    /// A capability or custom `llm_classifier` router, or a classifier overlay,
    /// with `classify_trigger = "new_session"` or `"user_turn"`.
    ///
    /// libsy latches every completed decision into affinity: under
    /// `new_session` the first one wins and every later completed drive
    /// replays it, under `user_turn` each decision overwrites the last. Either
    /// way the target of the latest completed drive whose served target is
    /// libsy's own selection *is* the assignment afterwards, so each such drive
    /// overwrites the record. Two first turns racing on one session can leave
    /// the record naming the loser under `new_session`; the next completed turn
    /// replays the winner and corrects it.
    Affinity,
    /// A `type = "composite"` router's retained tier.
    ///
    /// Recorded only from the two outcomes that show the tier libsy kept: a
    /// fall-open that served the retained tier (`retained`), and a judge
    /// verdict that set the tier and was served (`llm-classifier`). A stage
    /// signal (`override`, `dimensions`, `capable_hold`, `ambiguous`) or the
    /// picker's `fall_open` serves a tier without saying what is retained —
    /// libsy's stage router overwrites the judge's evidence with its own — so
    /// those leave the record unchanged. The record can therefore lag libsy's
    /// by the turns between two revealing outcomes; a probe in that gap answers
    /// from the last tier it was shown.
    CompositeTier,
    /// A `mode = "escalation"` router's latch onto `strong`.
    ///
    /// libsy keeps the latch in `FallThrough` session state keyed by the
    /// session id alone, so a session's parent and every child share it, and
    /// drops it after [`LATCH_IDLE_TTL`] idle. A completed escalation drive
    /// that served the latch or confirmed an escalation sets it; any other
    /// completed outcome — the weak tier served, a fail-open, a fallback, an
    /// evidence string this build does not know — clears it.
    EscalationLatch { strong: String },
}

/// One session's retained target, and when a real drive last showed it.
#[derive(Debug, Clone)]
struct Held {
    target: String,
    last_seen: Instant,
}

/// The per-entry record. See [the module docs](self).
#[derive(Debug)]
pub(crate) struct Retention {
    policy: Policy,
    held: Mutex<HashMap<BudgetKey, Held>>,
}

impl Retention {
    pub(super) fn new(policy: Policy) -> Self {
        Self {
            policy,
            held: Mutex::new(HashMap::new()),
        }
    }

    /// Record what one completed ungated drive retained.
    ///
    /// `selected_by_libsy` is false when `decide` substituted the entry's
    /// fail-open target for a missing or out-of-set selection: that target is
    /// shunt's, not libsy's, and says nothing about what the algorithm kept.
    pub(super) fn observe(
        &self,
        hints: &RouterContext<'_>,
        decision: &DrivenDecision,
        selected_by_libsy: bool,
        now: Instant,
    ) {
        if !selected_by_libsy {
            return;
        }
        let reveals = match self.policy {
            Policy::Affinity => true,
            Policy::CompositeTier => matches!(
                decision.source,
                RouteSource::DrivenRetained
                    | RouteSource::Stage(_, StageSource::Scorer(DecisionSource::LlmClassifier))
            ),
            Policy::None | Policy::EscalationLatch { .. } => false,
        };
        if let (true, Some(key)) = (reveals, identity_key(hints)) {
            self.record(key, &decision.target, now);
        }
    }

    /// Record whether one completed escalation drive left the session latched.
    pub(super) fn observe_escalation(
        &self,
        hints: &RouterContext<'_>,
        latched: bool,
        now: Instant,
    ) {
        let Policy::EscalationLatch { strong } = &self.policy else {
            return;
        };
        let Some(key) = JudgeBudget::session_key(hints) else {
            return;
        };
        if latched {
            self.record(key, strong, now);
        } else {
            self.lock().remove(&key);
        }
    }

    /// What a probe on this turn's identity resolves to, or `None` when no
    /// live record exists and the no-model-call decision answers.
    ///
    /// A pure read: it refreshes nothing, inserts nothing, and evicts nothing,
    /// so a stream of probes can neither keep a latch alive past its idle
    /// window nor push a real session's record out of the map.
    pub(crate) fn held(&self, hints: &RouterContext<'_>, now: Instant) -> Option<Retained> {
        let (key, source) = match self.policy {
            Policy::None => return None,
            Policy::Affinity | Policy::CompositeTier => {
                (identity_key(hints)?, RouteSource::DrivenRetained)
            }
            Policy::EscalationLatch { .. } => (
                JudgeBudget::session_key(hints)?,
                RouteSource::EscalationLatch,
            ),
        };
        let held = self.lock();
        let held = held.get(&key)?;
        if matches!(self.policy, Policy::EscalationLatch { .. })
            && now.saturating_duration_since(held.last_seen) > LATCH_IDLE_TTL
        {
            return None;
        }
        Some(Retained {
            target: held.target.clone(),
            source,
        })
    }

    fn record(&self, key: BudgetKey, target: &str, now: Instant) {
        let mut held = self.lock();
        // No class can hold `HARD_CAP` keys while the whole map holds fewer.
        if held.len() >= HARD_CAP && !held.contains_key(&key) {
            make_room(&mut held, key.is_delegated());
        }
        held.insert(
            key,
            Held {
                target: target.to_string(),
                last_seen: now,
            },
        );
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<BudgetKey, Held>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Entries currently held, for the unit tests.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.lock().len()
    }

    /// The cap, for the unit tests.
    #[cfg(test)]
    pub(super) const fn hard_cap() -> usize {
        HARD_CAP
    }
}

/// The `(session, agent)` identity libsy's affinity and composite tiers key
/// on, or `None` when libsy retains nothing for this turn.
///
/// libsy's `RoutingIdentity::from_request` has no identity for a delegated
/// turn that names no agent, so an unnamed delegate is never recorded — and a
/// sessionless turn is not either (see the module docs).
fn identity_key(hints: &RouterContext<'_>) -> Option<BudgetKey> {
    if BudgetScope::of(hints) == BudgetScope::UnnamedDelegate {
        return None;
    }
    JudgeBudget::key_for(hints)
}

/// Free one slot in `delegated`'s class if it is at [`HARD_CAP`], by removing
/// its least recently seen entry — `budget.rs`'s `make_room` without the
/// expiry pass, since nothing here but a latch has a clock and an idle latch
/// is exactly what this removes first.
fn make_room(held: &mut HashMap<BudgetKey, Held>, delegated: bool) {
    let in_class = held
        .keys()
        .filter(|key| key.is_delegated() == delegated)
        .count();
    if in_class < HARD_CAP {
        return;
    }
    let idlest = held
        .iter()
        .filter(|(key, _)| key.is_delegated() == delegated)
        .min_by_key(|(_, entry)| entry.last_seen)
        .map(|(key, _)| *key);
    if let Some(idlest) = idlest {
        held.remove(&idlest);
    }
}
