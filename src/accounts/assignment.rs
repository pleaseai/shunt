//! Session placement for pooled providers (ADR-0008 §2–§3).
//!
//! Pure decision logic over the per-request health snapshot that
//! `AccountPool::select_order_inner` builds: whether a bound account is still
//! eligible for this request, and which identity a strategy places a new or
//! displaced session on. Everything here is indexed by *slot*, the position
//! of an identity representative in the pool's `ident_reps` list, which is
//! configuration order over distinct identities.

use std::fmt;
use std::time::Duration;

use crate::config::PoolAssignment;

use super::session_binding::SessionHash;
use super::AccountKey;

/// How long an account must stay continuously soft-near before a bound
/// session leaves it (FR-5). Every other ineligibility moves immediately.
pub(crate) const SOFT_NEAR_DEBOUNCE: Duration = Duration::from_secs(60);

/// Why a session left its bound account. The labels follow issue #872.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SwitchReason {
    /// The bound account was soft-near past the debounce, hard-over,
    /// quota-rejected, capped, paused or disabled.
    Proactive,
    /// The bound account was cooling down for the request's scope.
    Cooldown,
    /// The bound account was attempted and failed over, or left the pool.
    Failover,
}

impl SwitchReason {
    /// Metric label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proactive => "proactive",
            Self::Cooldown => "cooldown",
            Self::Failover => "failover",
        }
    }
}

/// What selection observed about a session's binding, handed back to
/// [`super::AccountPool::commit_session_binding`] by the adapter that served
/// the request. Carries no raw session id.
#[derive(Clone)]
pub struct BindingTicket {
    pub(super) provider: String,
    pub(super) session: SessionHash,
    /// Generation of the binding selection observed; 0 when unbound.
    pub(super) generation: u64,
    /// The account the session was bound to before this request.
    pub(super) prior: Option<AccountKey>,
    /// Why `prior` could not serve this request, when it could not (or has
    /// left the pool). A soft-near `prior` counts only when a healthy
    /// alternative existed.
    pub(super) prior_ineligible: Option<SwitchReason>,
}

impl BindingTicket {
    /// Whether the session was bound before this request.
    pub fn is_bound(&self) -> bool {
        self.prior.is_some()
    }

    /// Why the bound account was ineligible for this request, if it was.
    pub fn prior_ineligible(&self) -> Option<SwitchReason> {
        self.prior_ineligible
    }

    /// Whether the session was bound to `account` before this request, so an
    /// adapter can tell that its attempt on the bound account failed over.
    pub fn is_bound_to(&self, account: &crate::config::AccountConfig) -> bool {
        self.prior
            .as_ref()
            .is_some_and(|prior| *prior == super::account_key(&self.provider, account))
    }
}

// Hand-written so a Debug print never shows the session hash.
impl fmt::Debug for BindingTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BindingTicket")
            .field("provider", &self.provider)
            .field("generation", &self.generation)
            .field("bound", &self.prior.is_some())
            .field("prior_ineligible", &self.prior_ineligible)
            .finish()
    }
}

/// Result of [`super::AccountPool::commit_session_binding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingCommit {
    /// The session had no binding; it is now bound to the serving account.
    /// A placement, not a switch.
    Placed,
    /// The bound account served; its `last_used` was refreshed.
    Touched,
    /// The binding moved to the serving account for this reason.
    Moved(SwitchReason),
    /// Another account served for a reason that does not move a binding
    /// (admission skip, re-probe promotion).
    Unchanged,
    /// The binding changed since selection observed it; nothing was written.
    Stale,
}

/// One identity representative's health for this request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SlotHealth {
    /// In the rotation: enabled, not paused for the provider, not capped.
    pub(crate) selectable: bool,
    /// Cooling down for this request's scope.
    pub(crate) cooling: bool,
    /// Any quota pressure (soft-near, projected burn, rejection, hard-over).
    pub(crate) near: bool,
    /// Quota-rejected or past the hard threshold: no debounce applies.
    pub(crate) immediate: bool,
    /// How long the account has been continuously soft-near, when it is.
    pub(crate) soft_near_for: Option<Duration>,
    pub(crate) priority: u32,
    /// Governing weekly reset (unix seconds), when known.
    pub(crate) weekly_reset: Option<u64>,
    /// Rendezvous weight of this session on this identity.
    pub(crate) weight: u64,
}

impl SlotHealth {
    /// Available and under every threshold: a placement target.
    fn healthy(&self) -> bool {
        self.selectable && !self.cooling && !self.near
    }

    /// `None` when a session bound here may keep using it for this request.
    fn ineligibility(&self) -> Option<SwitchReason> {
        if !self.selectable {
            return Some(SwitchReason::Proactive);
        }
        if self.cooling {
            return Some(SwitchReason::Cooldown);
        }
        if self.immediate {
            return Some(SwitchReason::Proactive);
        }
        match self.soft_near_for {
            Some(held) if self.near && held >= SOFT_NEAR_DEBOUNCE => Some(SwitchReason::Proactive),
            _ => None,
        }
    }
}

/// Where the session's binding points, relative to this request's slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bound {
    Unbound,
    /// Bound to an identity no longer in the pool's account list.
    Absent,
    At(usize),
}

/// The head of a session request's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    /// Slot that must lead the order, when one was chosen.
    pub(crate) head: Option<usize>,
    /// Slot the rotation starts from (the head, or the session's own slot
    /// when no head was chosen).
    pub(crate) start: usize,
    /// The head is the session's first choice and healthy, so the order is
    /// the rotation from it, exactly as today's sticky fast path.
    pub(crate) fast: bool,
    pub(crate) prior_ineligible: Option<SwitchReason>,
}

/// Decide the head of the order for one session request over `len` slots,
/// reading each slot's health through `slot` (no allocation on the hot path).
/// `weight` reads only a slot's rendezvous weight, so the common `spread`
/// case of a healthy first choice builds one [`SlotHealth`], not `len`.
pub(crate) fn place(
    strategy: PoolAssignment,
    len: usize,
    slot: impl Fn(usize) -> SlotHealth,
    weight: impl Fn(usize) -> u64,
    bound: Bound,
) -> Placement {
    debug_assert!(len > 0);
    let slots = || (0..len).map(|index| (index, slot(index)));
    match bound {
        Bound::At(bound_slot) => {
            let health = slot(bound_slot);
            match health.ineligibility() {
                None => Placement {
                    head: Some(bound_slot),
                    start: bound_slot,
                    fast: health.healthy(),
                    prior_ineligible: None,
                },
                Some(reason) => match move_target(strategy, slots(), bound_slot) {
                    Some(target) => Placement {
                        head: Some(target),
                        start: target,
                        fast: false,
                        prior_ineligible: Some(reason),
                    },
                    // No healthy alternative: today's order. Only a soft-near
                    // move needs a healthy target (FR-5); every other
                    // ineligibility still moves the binding if another
                    // account serves.
                    None => Placement {
                        head: None,
                        start: bound_slot,
                        fast: false,
                        prior_ineligible: (!health.selectable
                            || health.cooling
                            || health.immediate)
                            .then_some(reason),
                    },
                },
            }
        }
        Bound::Unbound | Bound::Absent => {
            // The session's first choice with health ignored, the analogue of
            // today's sticky slot. When it is healthy it is also the
            // placement; otherwise it seeds the rotation if nothing is.
            let home = match strategy {
                PoolAssignment::Spread => best_weight((0..len).map(|index| (index, weight(index)))),
                PoolAssignment::SoonestWeeklyReset => soonest_reset(slots()),
            }
            .unwrap_or(0);
            let home_healthy = slot(home).healthy();
            let target = if home_healthy {
                Some(home)
            } else {
                first_choice(strategy, slots().filter(|(_, health)| health.healthy()))
            };
            Placement {
                head: target,
                start: target.unwrap_or(home),
                fast: home_healthy,
                // A binding to a removed account can never serve again, so it
                // moves whatever serves next (AC-018).
                prior_ineligible: (bound == Bound::Absent).then_some(SwitchReason::Failover),
            }
        }
    }
}

/// First placement: `spread` takes the highest weight, priority ignored;
/// `soonest_weekly_reset` orders by priority, then weekly reset.
fn first_choice(
    strategy: PoolAssignment,
    candidates: impl Iterator<Item = (usize, SlotHealth)>,
) -> Option<usize> {
    match strategy {
        PoolAssignment::Spread => {
            best_weight(candidates.map(|(slot, health)| (slot, health.weight)))
        }
        PoolAssignment::SoonestWeeklyReset => soonest_reset(candidates),
    }
}

/// A move: the strategy's rule within the healthy slots other than `from`;
/// `spread` restricts itself to the best priority tier first.
fn move_target(
    strategy: PoolAssignment,
    slots: impl Iterator<Item = (usize, SlotHealth)> + Clone,
    from: usize,
) -> Option<usize> {
    let healthy = slots.filter(move |&(slot, health)| slot != from && health.healthy());
    match strategy {
        PoolAssignment::Spread => {
            let tier = healthy.clone().map(|(_, health)| health.priority).min()?;
            best_weight(
                healthy
                    .filter(|(_, health)| health.priority == tier)
                    .map(|(slot, health)| (slot, health.weight)),
            )
        }
        PoolAssignment::SoonestWeeklyReset => soonest_reset(healthy),
    }
}

/// Highest weight; ties keep the earlier slot.
fn best_weight(candidates: impl Iterator<Item = (usize, u64)>) -> Option<usize> {
    candidates
        .fold(
            None,
            |best: Option<(usize, u64)>, (slot, weight)| match best {
                Some((_, top)) if top >= weight => best,
                _ => Some((slot, weight)),
            },
        )
        .map(|(slot, _)| slot)
}

/// Priority, then weekly reset ascending with unknown last, then slot order.
fn soonest_reset(candidates: impl Iterator<Item = (usize, SlotHealth)>) -> Option<usize> {
    candidates
        .min_by_key(|&(slot, health)| {
            (
                health.priority,
                health.weekly_reset.is_none(),
                health.weekly_reset,
                slot,
            )
        })
        .map(|(slot, _)| slot)
}

#[cfg(test)]
mod tests;
