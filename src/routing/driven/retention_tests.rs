//! The retention record a `count_tokens` probe resolves against (issue #647):
//! which outcomes each [`Policy`] records, which identity it keys them under,
//! and that the probe's read leaves the record exactly as it found it.
//!
//! `tests/driven_lane/probe.rs` covers the whole probe instead; these pin the
//! per-form rules a live test would observe only through one upstream.

use std::time::{Duration, Instant};

use switchyard_libsy::DecisionSource;

use super::drive::DrivenDecision;
use super::retention::{Policy, Retained, Retention};
use crate::routing::context::{RequestClass, RouterContext};
use crate::routing::outcome::RouteSource;
use crate::routing::stage::{StageSource, StageTier};

/// A turn's hints — a session, an agent id, and a request class, each only
/// when given — built directly, as `budget_tests.rs` builds them.
fn hints<'a>(
    session: Option<&'a str>,
    agent: Option<&'a str>,
    class: Option<RequestClass>,
) -> RouterContext<'a> {
    RouterContext {
        session_id: session,
        agent_id: agent,
        request_class: class,
        ..RouterContext::default()
    }
}

fn decision(target: &str, source: RouteSource) -> DrivenDecision {
    DrivenDecision {
        target: target.to_string(),
        source,
        judge_outcome: "decided",
    }
}

fn retained(target: &str, source: RouteSource) -> Option<Retained> {
    Some(Retained {
        target: target.to_string(),
        source,
    })
}

/// Affinity records every completed drive whose target libsy selected, per
/// `(session, agent)`, and nothing for a turn libsy keeps no identity for; the
/// probe's read inserts nothing.
///
/// Non-vacuity: drop the `selected_by_libsy` early return in `observe` and the
/// first `held` goes red; drop the `UnnamedDelegate` check in `identity_key`
/// and the unnamed-delegate assertion does.
#[test]
fn affinity_records_libsys_own_selections_per_session_and_agent() {
    let retention = Retention::new(Policy::Affinity);
    let now = Instant::now();
    let parent = hints(Some("session-a"), None, None);

    retention.observe(
        &parent,
        &decision("strong", RouteSource::DrivenFailOpen),
        false,
        now,
    );
    assert_eq!(
        retention.held(&parent, now),
        None,
        "shunt's own fail-open substitution"
    );
    assert_eq!(
        retention.len(),
        0,
        "a read of a missing key inserts nothing"
    );

    retention.observe(&parent, &decision("weak", RouteSource::Driven), true, now);
    assert_eq!(
        retention.held(&parent, now),
        retained("weak", RouteSource::DrivenRetained)
    );
    retention.observe(
        &parent,
        &decision("strong", RouteSource::DrivenFailOpen),
        true,
        now,
    );
    assert_eq!(
        retention.held(&parent, now),
        retained("strong", RouteSource::DrivenRetained),
        "libsy latches a fail-open default too, so it overwrites"
    );

    let child = hints(
        Some("session-a"),
        Some("agent-1"),
        Some(RequestClass::Subagent),
    );
    assert_eq!(
        retention.held(&child, now),
        None,
        "a named child is its own key"
    );
    retention.observe(&child, &decision("weak", RouteSource::Driven), true, now);
    assert_eq!(
        retention.held(&child, now),
        retained("weak", RouteSource::DrivenRetained)
    );
    assert_eq!(
        retention.held(&parent, now),
        retained("strong", RouteSource::DrivenRetained),
        "the child's record leaves the parent's alone"
    );

    let unnamed = hints(Some("session-a"), None, Some(RequestClass::Subagent));
    retention.observe(&unnamed, &decision("weak", RouteSource::Driven), true, now);
    assert_eq!(
        retention.held(&unnamed, now),
        None,
        "an unnamed delegate is never held"
    );
    let sessionless = hints(None, None, None);
    retention.observe(
        &sessionless,
        &decision("weak", RouteSource::Driven),
        true,
        now,
    );
    assert_eq!(
        retention.held(&sessionless, now),
        None,
        "a sessionless turn is never held"
    );

    assert_eq!(retention.len(), 2, "the parent and the named child only");
    let _ = retention.held(&parent, now);
    let _ = retention.held(&unnamed, now);
    assert_eq!(retention.len(), 2, "reading twice changes nothing");
}

/// A composite records the tier only from the outcomes that show what libsy
/// retained: a `retained` fall-open and a served judge verdict.
///
/// Non-vacuity: widen `CompositeTier`'s `matches!` to every source and the
/// `override` assertion goes red.
#[test]
fn a_composite_records_only_the_outcomes_that_reveal_its_tier() {
    let retention = Retention::new(Policy::CompositeTier);
    let now = Instant::now();
    let hints = hints(Some("session-a"), None, None);
    let stage = |tier, source| RouteSource::Stage(tier, StageSource::Scorer(source));

    retention.observe(
        &hints,
        &decision(
            "efficient",
            stage(StageTier::Efficient, DecisionSource::LlmClassifier),
        ),
        true,
        now,
    );
    assert_eq!(
        retention.held(&hints, now),
        retained("efficient", RouteSource::DrivenRetained)
    );

    for ignored in [
        stage(StageTier::Capable, DecisionSource::Override),
        RouteSource::DrivenFailOpen,
    ] {
        retention.observe(&hints, &decision("capable", ignored), true, now);
        assert_eq!(
            retention.held(&hints, now),
            retained("efficient", RouteSource::DrivenRetained),
            "{ignored:?} leaves the record unchanged"
        );
    }

    retention.observe(
        &hints,
        &decision("capable", RouteSource::DrivenRetained),
        true,
        now,
    );
    assert_eq!(
        retention.held(&hints, now),
        retained("capable", RouteSource::DrivenRetained)
    );
}

/// The escalation latch is keyed by session alone, set by a latched outcome,
/// cleared by any other, and expires after an idle hour that a read does not
/// extend.
///
/// Non-vacuity: key `observe_escalation` and `held` by `identity_key` instead
/// of `session_key` and the child assertion goes red; drop the TTL check in
/// `held` and the idle assertion does.
#[test]
fn an_escalation_latch_is_per_session_cleared_and_idle_expired() {
    let retention = Retention::new(Policy::EscalationLatch {
        strong: "strong".to_string(),
    });
    let start = Instant::now();
    let parent = hints(Some("session-a"), None, None);
    let child = hints(
        Some("session-a"),
        Some("agent-1"),
        Some(RequestClass::Subagent),
    );

    retention.observe_escalation(&parent, true, start);
    assert_eq!(
        retention.held(&parent, start),
        retained("strong", RouteSource::EscalationLatch)
    );
    assert_eq!(
        retention.held(&child, start),
        retained("strong", RouteSource::EscalationLatch),
        "a child of the same session shares the latch"
    );

    retention.observe_escalation(&child, false, start);
    assert_eq!(
        retention.held(&parent, start),
        None,
        "a weak outcome clears it"
    );

    retention.observe_escalation(&parent, true, start);
    let hour = Duration::from_secs(60 * 60);
    assert!(
        retention.held(&parent, start + hour).is_some(),
        "the hour itself is live"
    );
    let idle = start + hour + Duration::from_secs(1);
    assert_eq!(retention.held(&parent, idle), None, "past the idle hour");
    assert_eq!(
        retention.held(&parent, start + hour),
        retained("strong", RouteSource::EscalationLatch),
        "the expired read neither refreshed nor removed it"
    );
    assert_eq!(
        retention.held(&parent, idle),
        None,
        "and did not refresh it either"
    );
}

/// `every_request` and `advisor` retain nothing a probe could read.
///
/// Non-vacuity: return `true` for `Policy::None` in `observe`'s `reveals` and
/// this goes red.
#[test]
fn the_none_policy_never_holds_anything() {
    let retention = Retention::new(Policy::None);
    let now = Instant::now();
    let hints = hints(Some("session-a"), None, None);
    retention.observe(&hints, &decision("weak", RouteSource::Driven), true, now);
    retention.observe_escalation(&hints, true, now);
    assert_eq!(retention.held(&hints, now), None);
    assert_eq!(retention.len(), 0);
}

/// The record is bounded per class, evicting the least recently seen entry,
/// and a delegated fan-out never evicts a parent.
///
/// Non-vacuity: drop the `make_room` call in `record` and the size assertion
/// goes red.
#[test]
fn the_record_is_bounded_per_class() {
    let retention = Retention::new(Policy::Affinity);
    let start = Instant::now();
    let cap = Retention::hard_cap();
    let at = |i: usize| start + Duration::from_millis(i as u64);
    for i in 0..cap {
        let session = format!("session-{i}");
        let hints = hints(Some(&session), None, None);
        retention.observe(&hints, &decision("weak", RouteSource::Driven), true, at(i));
    }
    for i in 0..=cap {
        let agent = format!("agent-{i}");
        let hints = hints(
            Some("session-0"),
            Some(&agent),
            Some(RequestClass::Subagent),
        );
        retention.observe(&hints, &decision("weak", RouteSource::Driven), true, at(i));
    }
    assert_eq!(retention.len(), 2 * cap, "each class holds at most the cap");
    let first = hints(Some("session-0"), None, None);
    assert!(
        retention.held(&first, at(cap)).is_some(),
        "the delegated fan-out evicted no parent"
    );
    let idlest = hints(
        Some("session-0"),
        Some("agent-0"),
        Some(RequestClass::Subagent),
    );
    assert_eq!(
        retention.held(&idlest, at(cap)),
        None,
        "the least recently seen delegate went first"
    );
}
