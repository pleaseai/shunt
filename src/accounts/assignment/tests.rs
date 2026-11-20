use super::*;

fn healthy(weight: u64) -> SlotHealth {
    SlotHealth {
        selectable: true,
        cooling: false,
        near: false,
        immediate: false,
        soft_near_for: None,
        priority: 100,
        weekly_reset: None,
        weight,
    }
}

fn soft_near(weight: u64, held: Duration) -> SlotHealth {
    SlotHealth {
        near: true,
        soft_near_for: Some(held),
        ..healthy(weight)
    }
}

#[test]
fn spread_places_on_highest_weight_ignoring_priority() {
    let mut slots = [healthy(10), healthy(30), healthy(20)];
    slots[1].priority = 500;
    let placement = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::Unbound,
    );
    assert_eq!(placement.head, Some(1));
    assert_eq!(placement.start, 1);
    assert!(placement.fast);
    assert_eq!(placement.prior_ineligible, None);
}

#[test]
fn spread_placement_skips_unhealthy_first_choice_without_fast_path() {
    let mut slots = [healthy(10), healthy(30), healthy(20)];
    slots[1].cooling = true;
    let placement = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::Unbound,
    );
    assert_eq!(placement.head, Some(2));
    assert!(!placement.fast);
}

#[test]
fn no_healthy_candidate_leaves_todays_order_and_binding() {
    let mut slots = [healthy(10), healthy(30)];
    slots[0].cooling = true;
    slots[1].cooling = true;
    let unbound = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::Unbound,
    );
    assert_eq!(unbound.head, None);
    assert_eq!(unbound.start, 1, "seeded at the session's own first choice");
    let bound = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::At(0),
    );
    assert_eq!(bound.head, None);
    assert_eq!(bound.start, 0);
    assert_eq!(
        bound.prior_ineligible,
        Some(SwitchReason::Cooldown),
        "AC-004 still holds: nothing commits unless another account serves"
    );
}

#[test]
fn paused_bound_slot_keeps_prior_ineligible_when_alternatives_are_soft_near() {
    let mut slots = [
        healthy(10),
        soft_near(30, Duration::from_secs(5)),
        soft_near(20, Duration::from_secs(5)),
    ];
    slots[0].selectable = false;
    let placement = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::At(0),
    );
    assert_eq!(placement.head, None);
    assert_eq!(placement.start, 0);
    assert_eq!(placement.prior_ineligible, Some(SwitchReason::Proactive));
}

#[test]
fn soft_near_bound_slot_without_healthy_alternative_stays_bound() {
    let slots = [
        soft_near(10, Duration::from_secs(61)),
        soft_near(30, Duration::from_secs(5)),
    ];
    let placement = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::At(0),
    );
    assert_eq!(placement.head, Some(0));
    assert_eq!(placement.start, 0);
    assert_eq!(
        placement.prior_ineligible, None,
        "FR-5 needs a healthy target"
    );
}

#[test]
fn bound_soft_near_is_debounced_for_sixty_seconds() {
    let mut slots = [
        soft_near(10, Duration::from_secs(59)),
        healthy(30),
        healthy(20),
    ];
    let placement = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::At(0),
    );
    assert_eq!(placement.head, Some(0), "AC-006");
    assert!(!placement.fast, "a near head keeps the failover tail");
    assert_eq!(placement.prior_ineligible, None);

    slots[0].soft_near_for = Some(SOFT_NEAR_DEBOUNCE);
    let placement = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::At(0),
    );
    assert_eq!(placement.head, Some(1));
    assert_eq!(placement.prior_ineligible, Some(SwitchReason::Proactive));
}

#[test]
fn immediate_ineligibility_reasons() {
    let base = || [healthy(10), healthy(30)];
    type Break = fn(&mut SlotHealth);
    let cases: [(Break, SwitchReason); 4] = [
        (|s| s.cooling = true, SwitchReason::Cooldown),
        (|s| s.selectable = false, SwitchReason::Proactive),
        (
            |s| {
                s.near = true;
                s.immediate = true;
            },
            SwitchReason::Proactive,
        ),
        // Cooling and capped together: the cooldown reports first only when
        // selectable, so an unselectable account is proactive.
        (
            |s| {
                s.selectable = false;
                s.cooling = true;
            },
            SwitchReason::Proactive,
        ),
    ];
    for (mutate, reason) in cases {
        let mut slots = base();
        mutate(&mut slots[0]);
        let placement = place(
            PoolAssignment::Spread,
            slots.len(),
            |i| slots[i],
            |i| slots[i].weight,
            Bound::At(0),
        );
        assert_eq!(placement.head, Some(1));
        assert_eq!(placement.prior_ineligible, Some(reason));
        assert!(!placement.fast);
    }
}

#[test]
fn spread_move_stays_in_best_priority_tier() {
    let mut slots = [healthy(0), healthy(99), healthy(5), healthy(7)];
    slots[0].cooling = true;
    slots[1].priority = 200;
    let placement = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::At(0),
    );
    assert_eq!(
        placement.head,
        Some(3),
        "highest weight within priority 100"
    );
}

#[test]
fn absent_binding_is_placed_again_as_failover() {
    let slots = [healthy(10), healthy(30)];
    let placement = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::Absent,
    );
    assert_eq!(placement.head, Some(1));
    assert!(placement.fast);
    assert_eq!(placement.prior_ineligible, Some(SwitchReason::Failover));
}

#[test]
fn soonest_weekly_reset_orders_by_priority_then_reset_then_slot() {
    let mut slots = [healthy(0), healthy(0), healthy(0), healthy(0)];
    slots[0].weekly_reset = None;
    slots[1].weekly_reset = Some(900);
    slots[2].weekly_reset = Some(500);
    slots[3].weekly_reset = Some(100);
    slots[3].priority = 200;
    let placement = place(
        PoolAssignment::SoonestWeeklyReset,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::Unbound,
    );
    assert_eq!(placement.head, Some(2));

    slots[2].weekly_reset = Some(900);
    let placement = place(
        PoolAssignment::SoonestWeeklyReset,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::Unbound,
    );
    assert_eq!(
        placement.head,
        Some(1),
        "equal resets fall back to slot order"
    );

    slots[1].weekly_reset = None;
    slots[2].weekly_reset = None;
    let placement = place(
        PoolAssignment::SoonestWeeklyReset,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::Unbound,
    );
    assert_eq!(
        placement.head,
        Some(0),
        "priority outranks a known reset; unknown resets tie-break on slot order"
    );
}

#[test]
fn soonest_weekly_reset_never_moves_a_healthy_binding() {
    let mut slots = [healthy(0), healthy(0)];
    slots[0].weekly_reset = Some(900);
    slots[1].weekly_reset = Some(100);
    let placement = place(
        PoolAssignment::SoonestWeeklyReset,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::At(0),
    );
    assert_eq!(placement.head, Some(0), "AC-013");
    // The head stays the binding (AC-013), but a binding that is not the strategy's first choice takes the sorted failover order, not the fast path.
    assert!(!placement.fast);
}

#[test]
fn only_a_healthy_bound_first_choice_takes_the_fast_path() {
    let slots = [healthy(10), healthy(30), healthy(20)];
    // Bound to slot 2 after moving off the first choice (slot 1).
    let moved = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::At(2),
    );
    assert_eq!(moved.head, Some(2));
    assert_eq!(moved.start, 2);
    assert!(!moved.fast, "a moved binding takes the failover order");
    assert_eq!(moved.prior_ineligible, None);

    let home = place(
        PoolAssignment::Spread,
        slots.len(),
        |i| slots[i],
        |i| slots[i].weight,
        Bound::At(1),
    );
    assert_eq!(home.head, Some(1));
    assert!(home.fast, "a healthy bound first choice is the fast path");
}

#[test]
fn switch_reason_labels() {
    assert_eq!(SwitchReason::Proactive.as_str(), "proactive");
    assert_eq!(SwitchReason::Cooldown.as_str(), "cooldown");
    assert_eq!(SwitchReason::Failover.as_str(), "failover");
}
