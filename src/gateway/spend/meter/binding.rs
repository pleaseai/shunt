//! The binding cap: the one cap a principal's rate-limit headers describe.
//!
//! Follows the reference gateway's fold (see the track's
//! `reference-gateway.md`): per period with a cap, `exceeded = spent >= cap`
//! and `utilization = spent / cap` (1 for a zero cap). Folding daily, weekly,
//! monthly in that order:
//!
//! - one exceeded, the other not: the exceeded one;
//! - both exceeded: the one that resets later (an equal reset keeps the
//!   earlier period, which is also the period [`Check::Blocked`] names);
//! - neither: the higher utilization, a tie keeping the later period.
//!
//! Utilization is compared and rounded on the integer femto amounts, so no
//! float rounding moves a boundary.
//!
//! [`Check::Blocked`]: super::Check::Blocked

use std::cmp::Ordering;

use super::super::store::Period;

/// One capped period's position against its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub period: Period,
    /// Period-to-date spend, femto-USD.
    pub spent: u128,
    /// The effective cap, femto-USD.
    pub cap: u128,
    /// Unix seconds at which the period's window resets.
    pub reset_at: u64,
}

/// A warning threshold, in hundredths: `100` (reached), `95` or `75`.
pub type Threshold = u32;

impl Binding {
    /// `spent >= cap`; a zero cap is always reached.
    pub fn exceeded(&self) -> bool {
        self.spent >= self.cap
    }

    /// Utilization in hundredths, rounded half up as the reference's
    /// `Math.round(u * 100)` and capped at `99` unless the cap is reached.
    pub fn utilization_hundredths(&self) -> u128 {
        let rounded = if self.cap == 0 {
            100
        } else {
            // round(spent * 100 / cap) = floor((spent * 200 + cap) / (2 * cap)).
            self.cap.checked_mul(2).map_or(0, |denominator| {
                self.spent
                    .saturating_mul(200)
                    .saturating_add(self.cap)
                    .checked_div(denominator)
                    .unwrap_or(0)
            })
        };
        if self.exceeded() {
            rounded
        } else {
            rounded.min(99)
        }
    }

    /// `100` when reached, else the first of 95 / 75 that the unrounded
    /// utilization strictly exceeds.
    pub fn threshold(&self) -> Option<Threshold> {
        if self.exceeded() {
            return Some(100);
        }
        [95, 75]
            .into_iter()
            .find(|percent| above_percent(self.spent, self.cap, *percent))
    }
}

/// `spent / cap > percent / 100`, exactly.
fn above_percent(spent: u128, cap: u128, percent: u32) -> bool {
    match cap.checked_mul(u128::from(percent)) {
        Some(bound) => spent.saturating_mul(100) > bound,
        // A cap this large is never 75% spent by a `u64` counter.
        None => false,
    }
}

/// Orders two non-exceeded utilizations `a.spent / a.cap` and
/// `b.spent / b.cap` by cross multiplication, falling back to floats only
/// where the products overflow.
fn compare_utilization(a: &Binding, b: &Binding) -> Ordering {
    match (a.spent.checked_mul(b.cap), b.spent.checked_mul(a.cap)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => {
            let ratio = |binding: &Binding| binding.spent as f64 / binding.cap as f64;
            ratio(a).partial_cmp(&ratio(b)).unwrap_or(Ordering::Equal)
        }
    }
}

/// Keeps `earlier` or `later` (the next period in fold order).
fn prefer(earlier: Binding, later: Binding) -> Binding {
    match (earlier.exceeded(), later.exceeded()) {
        (true, false) => earlier,
        (false, true) => later,
        (true, true) if later.reset_at > earlier.reset_at => later,
        (true, true) => earlier,
        (false, false) if compare_utilization(&earlier, &later) == Ordering::Greater => earlier,
        (false, false) => later,
    }
}

/// The binding cap among `capped` periods, given in daily, weekly, monthly
/// order. `None` when no period has a cap.
pub fn fold(capped: impl IntoIterator<Item = Binding>) -> Option<Binding> {
    capped.into_iter().reduce(prefer)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENT: u128 = 10_000_000_000_000;

    fn at(period: Period, spent_cents: u128, cap_cents: u128, reset_at: u64) -> Binding {
        Binding {
            period,
            spent: spent_cents * CENT,
            cap: cap_cents * CENT,
            reset_at,
        }
    }

    #[test]
    fn utilization_rounds_half_up_to_hundredths_and_caps_below_the_limit() {
        assert_eq!(at(Period::Daily, 82, 100, 0).utilization_hundredths(), 82);
        // 0.825 rounds up, 0.8249… down.
        let half = Binding {
            spent: 825,
            cap: 1000,
            ..at(Period::Daily, 0, 1, 0)
        };
        assert_eq!(half.utilization_hundredths(), 83);
        let below_half = Binding {
            spent: 8249,
            cap: 10_000,
            ..half
        };
        assert_eq!(below_half.utilization_hundredths(), 82);
        // 0.996 would round to 1.00 but is not reached: capped at 0.99.
        let nearly = Binding {
            spent: 996,
            cap: 1000,
            ..half
        };
        assert!(!nearly.exceeded());
        assert_eq!(nearly.utilization_hundredths(), 99);
        // Reached: no cap on the value.
        assert_eq!(at(Period::Daily, 100, 100, 0).utilization_hundredths(), 100);
        assert_eq!(at(Period::Daily, 120, 100, 0).utilization_hundredths(), 120);
        assert_eq!(at(Period::Daily, 0, 100, 0).utilization_hundredths(), 0);
    }

    #[test]
    fn zero_cap_is_full_utilization_and_exceeded() {
        let zero = at(Period::Weekly, 0, 0, 0);
        assert!(zero.exceeded());
        assert_eq!(zero.utilization_hundredths(), 100);
        assert_eq!(zero.threshold(), Some(100));
    }

    #[test]
    fn thresholds_are_strictly_greater_than() {
        assert_eq!(at(Period::Daily, 75, 100, 0).threshold(), None);
        let just_over = Binding {
            spent: 75 * CENT + 1,
            ..at(Period::Daily, 0, 100, 0)
        };
        assert_eq!(just_over.threshold(), Some(75));
        assert_eq!(at(Period::Daily, 95, 100, 0).threshold(), Some(75));
        assert_eq!(at(Period::Daily, 96, 100, 0).threshold(), Some(95));
        assert_eq!(at(Period::Daily, 100, 100, 0).threshold(), Some(100));
    }

    #[test]
    fn an_exceeded_period_beats_a_higher_unexceeded_one() {
        let daily = at(Period::Daily, 99, 100, 10);
        let weekly = at(Period::Weekly, 5, 5, 20);
        assert_eq!(fold([daily, weekly]), Some(weekly));
        let daily = at(Period::Daily, 7, 5, 10);
        let weekly = at(Period::Weekly, 99, 100, 20);
        assert_eq!(fold([daily, weekly]), Some(daily));
    }

    #[test]
    fn both_exceeded_keeps_the_later_reset_and_an_equal_reset_keeps_the_earlier() {
        let daily = at(Period::Daily, 5, 5, 10);
        let weekly = at(Period::Weekly, 9, 5, 20);
        let monthly = at(Period::Monthly, 5, 5, 15);
        assert_eq!(fold([daily, weekly, monthly]), Some(weekly));
        let same = at(Period::Monthly, 5, 5, 20);
        assert_eq!(fold([weekly, same]), Some(weekly));
    }

    #[test]
    fn neither_exceeded_keeps_the_higher_utilization_and_a_tie_the_later_period() {
        let daily = at(Period::Daily, 50, 100, 10);
        let weekly = at(Period::Weekly, 300, 1000, 20);
        assert_eq!(fold([daily, weekly]), Some(daily));
        let tie = at(Period::Weekly, 500, 1000, 20);
        assert_eq!(fold([daily, tie]), Some(tie));
        let monthly = at(Period::Monthly, 1, 1000, 30);
        assert_eq!(fold([daily, tie, monthly]), Some(tie));
    }

    #[test]
    fn no_capped_period_has_no_binding() {
        assert_eq!(fold([]), None);
    }
}
