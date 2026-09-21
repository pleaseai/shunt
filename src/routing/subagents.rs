//! The `[models.subagents]` overlay for delegated work (ADR-0005 §5, §11).
//!
//! A `Task` child, a hook agent, or a workflow sub-agent requesting an entry
//! that carries the overlay is diverted to the overlay's target — the
//! `by_type` entry for its `x-claude-code-agent-type`, else `target` — before
//! the entry's own router or map is consulted. Everything else requesting the
//! same id resolves the entry as if the table were absent: the parent's own
//! turns, and the harness-maintenance classes `compaction` and `auxiliary`,
//! which are delegated by no reading of the headers.
//!
//! This is the passthrough form on the pure lane. [`select`] reads the hints
//! and the config and nothing else — no store, no pin, no judge — so a child's
//! turn through the overlay leaves no trace a later turn could be steered by.

use crate::config::SubagentsConfig;
use crate::routing::context::RouterContext;
use crate::routing::outcome::RouteSource;

/// The overlay's answer for one request: `None` when the turn is not delegated
/// work and the entry resolves as if it had no overlay, or the target and the
/// route source when it is.
///
/// Delegation is [`RouterContext::is_delegated`]: `subagent` or `workflow` when
/// the class header is sent — the class is authoritative, so `main` with an
/// agent id is main traffic — and a non-blank agent id when it is not. That
/// fallback is the live path on every default deployment, where the class is
/// gated off and the agent id is not.
///
/// [`RouteSource::SubagentType`] marks a `by_type` hit and
/// [`RouteSource::Subagent`] the `target` fallback, so the two are readable
/// apart in the header and the metric — the same split `random` makes between
/// a fresh draw and a session-hashed arm.
pub(crate) fn select<'a>(
    overlay: &'a SubagentsConfig,
    hints: &RouterContext<'_>,
) -> Option<(&'a str, RouteSource)> {
    if !hints.is_delegated() {
        return None;
    }
    let (target, by_type) = overlay.target_for(hints.agent_type);
    Some((
        target,
        if by_type {
            RouteSource::SubagentType
        } else {
            RouteSource::Subagent
        },
    ))
}

#[cfg(test)]
mod resolve_tests;

/// One test per clause of the ADR-0005 §8 PR 3 definition of done, and one
/// negative test per excluded class.
///
/// Non-vacuity: make [`select`] ignore `is_delegated` and every `*_never_takes_
/// the_overlay` test goes red; make it ignore `agent_type` and
/// `a_subagent_routes_to_its_by_type_target` goes red on the source as well as
/// the target.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::context::RequestClass;

    fn overlay() -> SubagentsConfig {
        toml::from_str(
            r#"
            type = "passthrough"
            target = "claude-haiku-4-5"
            by_type = { Explore = "explore-target", custom = "custom-target" }
            "#,
        )
        .expect("the overlay parses")
    }

    fn hints<'a>(
        class: Option<RequestClass>,
        agent_id: Option<&'a str>,
        agent_type: Option<&'a str>,
    ) -> RouterContext<'a> {
        RouterContext {
            session_id: Some("session-a"),
            agent_id,
            request_class: class,
            agent_type,
            context_compacted: false,
        }
    }

    /// Clause 1, first half: a `subagent` request routes to its `by_type` target.
    #[test]
    fn a_subagent_routes_to_its_by_type_target() {
        let overlay = overlay();
        let hints = hints(
            Some(RequestClass::Subagent),
            Some("a7a11c2e22e29e67a"),
            Some("Explore"),
        );

        assert_eq!(
            select(&overlay, &hints),
            Some(("explore-target", RouteSource::SubagentType))
        );
    }

    /// Clause 1, second half: a `workflow` request whose type has no `by_type`
    /// entry routes to `target`.
    #[test]
    fn a_workflow_without_a_by_type_entry_routes_to_target() {
        let overlay = overlay();
        let hints = hints(
            Some(RequestClass::Workflow),
            Some("acf659811cf929e90"),
            Some("general-purpose"),
        );

        assert_eq!(
            select(&overlay, &hints),
            Some(("claude-haiku-4-5", RouteSource::Subagent))
        );
    }

    /// The default deployment: the class and type headers are gated off, the
    /// agent id is not. A child is still recognised and takes `target`.
    #[test]
    fn an_agent_id_without_a_class_routes_to_target() {
        let overlay = overlay();
        let hints = hints(None, Some("a6544583269e5f238"), None);

        assert_eq!(
            select(&overlay, &hints),
            Some(("claude-haiku-4-5", RouteSource::Subagent))
        );
    }

    /// A project agent's own name never reaches the wire; `custom` does, and it
    /// is the only key such an agent can match.
    #[test]
    fn a_custom_agent_matches_the_custom_key() {
        let overlay = overlay();
        let hints = hints(Some(RequestClass::Subagent), Some("id"), Some("custom"));

        assert_eq!(
            select(&overlay, &hints),
            Some(("custom-target", RouteSource::SubagentType))
        );
    }

    /// Keys are the literal header values: `explore` is not `Explore`.
    #[test]
    fn by_type_keys_are_case_sensitive() {
        let overlay = overlay();
        let hints = hints(Some(RequestClass::Subagent), Some("id"), Some("explore"));

        assert_eq!(
            select(&overlay, &hints),
            Some(("claude-haiku-4-5", RouteSource::Subagent))
        );
    }

    /// Clause 2: `main` with an agent id is main traffic. The class is
    /// authoritative when sent, whatever the agent-id fallback would say.
    #[test]
    fn main_with_an_agent_id_never_takes_the_overlay() {
        let overlay = overlay();
        let hints = hints(
            Some(RequestClass::Main),
            Some("a7a11c2e22e29e67a"),
            Some("Explore"),
        );

        assert_eq!(select(&overlay, &hints), None);
    }

    /// Clause 2: the compaction call is harness maintenance, not delegation —
    /// even when it carries an agent id and a type that `by_type` names.
    #[test]
    fn compaction_never_takes_the_overlay() {
        let overlay = overlay();
        let hints = hints(
            Some(RequestClass::Compaction),
            Some("a7a11c2e22e29e67a"),
            Some("Explore"),
        );

        assert_eq!(select(&overlay, &hints), None);
    }

    /// Clause 2: title generation and the other `auxiliary` calls likewise.
    #[test]
    fn auxiliary_never_takes_the_overlay() {
        let overlay = overlay();
        let hints = hints(
            Some(RequestClass::Auxiliary),
            Some("a7a11c2e22e29e67a"),
            Some("Explore"),
        );

        assert_eq!(select(&overlay, &hints), None);
    }

    /// The parent's own turn, and a hint-less request, see no overlay at all.
    #[test]
    fn a_parent_turn_never_takes_the_overlay() {
        let overlay = overlay();

        assert_eq!(
            select(&overlay, &hints(Some(RequestClass::Main), None, None)),
            None
        );
        assert_eq!(select(&overlay, &RouterContext::default()), None);
        // A blank agent id is not a child either.
        assert_eq!(select(&overlay, &hints(None, Some("  "), None)), None);
    }
}
