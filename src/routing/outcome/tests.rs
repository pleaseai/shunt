//! Coverage for the `RouteSource` label arms, including the conditional
//! router's. The labels are load-bearing metric and header values, so a
//! renamed arm should fail here rather than silently change a series.

use crate::routing::outcome::RouteSource;

#[test]
fn conditional_labels_are_stable() {
    assert_eq!(RouteSource::Conditional.as_label(), "conditional");
    assert_eq!(
        RouteSource::ConditionalDefault.as_label(),
        "conditional_default"
    );
}

#[test]
fn conditional_arms_have_no_stage_labels() {
    // The `shunt.stage_router.*` series must not gain rows for a router that
    // has no tier to report.
    assert!(RouteSource::Conditional.stage_labels().is_none());
    assert!(RouteSource::ConditionalDefault.stage_labels().is_none());
}
