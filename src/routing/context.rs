//! Request hints the router reads from Claude Code's gateway headers.
//!
//! Claude Code ≥ 2.1.273 describes each request to an LLM gateway with a small
//! set of `x-claude-code-*` headers (ADR-0005 §11). [`RouterContext`] is the
//! parsed form: everything a routing decision may key on that lives in the
//! headers rather than the body. It is built once per request, borrowed for
//! the length of the routing call, and nothing in it is stored — the store
//! hashes the two ids and keeps the digests.
//!
//! Every field has an "absent" branch equal to the behaviour before the hints
//! existed, because absent is the common case. Four of the five are gated
//! client-side: behind any non-Anthropic base URL — every shunt deployment —
//! they are off until the operator sets `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`.
//! The exception is `x-claude-code-agent-id`, which is not gated and has been
//! sent on every delegated turn since 2.1.139; a live capture
//! (`docs/notes/adr-0005-routing-live-captures.md`, fact (a)) found its
//! presence exactly coextensive with delegated work across 77 turns, which is
//! why it is the fallback the class-less case keys on.

use axum::http::HeaderMap;

/// Set by Claude Code on every request; the id `[[models]]` pins are keyed on.
pub(crate) const SESSION_ID_HEADER: &str = "x-claude-code-session-id";
/// Present on every turn of a delegated (`Task`) child and on none of the
/// parent's. Not gated: sent regardless of `CLAUDE_CODE_GATEWAY_HINT_HEADERS`.
pub(crate) const AGENT_ID_HEADER: &str = "x-claude-code-agent-id";
/// `main`, `subagent`, `workflow`, `compaction`, or `auxiliary`. Gated.
pub(crate) const REQUEST_CLASS_HEADER: &str = "x-claude-code-request-class";
/// `teammate`, a built-in agent id verbatim (`Explore`, `fork`, …), or
/// `custom`. Only on delegated turns. Gated.
pub(crate) const AGENT_TYPE_HEADER: &str = "x-claude-code-agent-type";
/// `manual`, `auto`, or `reactive`, sent **once** on the first `main` turn
/// after a compaction — the client consumes the flag when it reads it. Gated.
pub(crate) const CONTEXT_COMPACTED_HEADER: &str = "x-claude-code-context-compacted";

/// What kind of turn the client says this is (`x-claude-code-request-class`).
///
/// The literal values are the client's, read from the 2.1.274 binary and
/// confirmed on the wire for `main`, `subagent`, and `auxiliary`. A value not
/// in this set parses to "absent" rather than to a variant: the class is
/// authoritative only when shunt can read it, and a future value must fall back
/// to the agent-id rule rather than be mistaken for main traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestClass {
    /// The REPL main thread or the SDK.
    Main,
    /// Any `agent:*` source, or a hook agent.
    Subagent,
    /// A sub-agent under a workflow run.
    Workflow,
    /// The compaction call itself.
    Compaction,
    /// Title, summary, and other maintenance calls.
    Auxiliary,
}

impl RequestClass {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "main" => Some(Self::Main),
            "subagent" => Some(Self::Subagent),
            "workflow" => Some(Self::Workflow),
            "compaction" => Some(Self::Compaction),
            "auxiliary" => Some(Self::Auxiliary),
            _ => None,
        }
    }
}

/// The per-request hints, borrowed from the inbound headers.
///
/// `Default` is the hint-less request — a bare `curl`, or a Claude Code older
/// than the headers — and routes exactly as it did before any of this existed.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RouterContext<'a> {
    /// `x-claude-code-session-id`, as sent. Blank is filtered by the store, as
    /// it is everywhere else the id is a sticky key.
    pub session_id: Option<&'a str>,
    /// `x-claude-code-agent-id`, as sent. Opaque; stable across one child's
    /// turns and distinct per spawn, so hashing it partitions children from
    /// each other and from their parent.
    pub agent_id: Option<&'a str>,
    /// `x-claude-code-request-class`, when sent and recognised.
    pub request_class: Option<RequestClass>,
    /// `x-claude-code-agent-type`, as sent. The `[models.subagents]` `by_type`
    /// key (ADR-0005 §11): matched against the map's keys byte-for-byte, so
    /// `Explore` and `explore` are two types, and a project agent — sent as
    /// `custom` with its name withheld — can match only `custom`.
    pub agent_type: Option<&'a str>,
    /// Whether this turn carries `x-claude-code-context-compacted`. The header
    /// is one-shot, so this is true on exactly one turn per compaction; the
    /// store latches it onto the session pin for the turns that follow.
    pub context_compacted: bool,
}

impl<'a> RouterContext<'a> {
    /// Read the hints off a request's headers. Missing headers and values that
    /// are not visible ASCII read as absent.
    ///
    /// Values arrive percent-encoded for `%` and anything outside printable
    /// ASCII. Nothing here decodes them: the class and type are compared
    /// against ASCII literals, which an encoded byte can never equal, and the
    /// two ids are only ever hashed, where the encoded form is as stable across
    /// turns as the decoded one would be.
    pub(crate) fn from_headers(headers: &'a HeaderMap) -> Self {
        let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        Self {
            session_id: text(SESSION_ID_HEADER),
            agent_id: text(AGENT_ID_HEADER),
            request_class: text(REQUEST_CLASS_HEADER).and_then(RequestClass::parse),
            agent_type: text(AGENT_TYPE_HEADER),
            context_compacted: text(CONTEXT_COMPACTED_HEADER)
                .is_some_and(|value| !value.trim().is_empty()),
        }
    }

    /// A context carrying only a session id — what every caller before the
    /// hints existed had.
    #[cfg(test)]
    pub(crate) fn session(session_id: Option<&'a str>) -> Self {
        Self {
            session_id,
            ..Self::default()
        }
    }

    /// Whether this turn is delegated work — a `Task` child, a hook agent, a
    /// workflow sub-agent — rather than the session's own thread (ADR-0005 §5).
    ///
    /// The class is authoritative when sent: `subagent` and `workflow` are
    /// delegated, every other class is not, and `main` with an agent id is main
    /// traffic. That last combination has never been observed on the wire —
    /// across 77 captured turns no `main` turn carried an agent id — so the
    /// branch is defensive, keeping the header that names the class ahead of
    /// the one that merely correlates with it. Absent the class, a non-blank
    /// agent id is the rule, and behind a default shunt deployment that is the
    /// live path: the class is gated off and the agent id is not.
    pub(crate) fn is_delegated(&self) -> bool {
        match self.request_class {
            Some(RequestClass::Subagent | RequestClass::Workflow) => true,
            Some(RequestClass::Main | RequestClass::Compaction | RequestClass::Auxiliary) => false,
            None => self.non_blank_agent_id().is_some(),
        }
    }

    /// The agent id that scopes this turn's pin: the child's own id for
    /// delegated work, `None` for the parent — and for a delegated turn that
    /// sent no id, which then shares the parent's scope rather than a scope of
    /// its own. Two children never share an id, so the parent scope is the
    /// only one a missing id can safely fall into.
    pub(crate) fn pin_agent_id(&self) -> Option<&'a str> {
        if self.is_delegated() {
            self.non_blank_agent_id()
        } else {
            None
        }
    }

    fn non_blank_agent_id(&self) -> Option<&'a str> {
        self.agent_id.filter(|id| !id.trim().is_empty())
    }
}

/// Predicate tests.
///
/// Non-vacuity: make `is_delegated` return `self.agent_id.is_some()` and
/// `main_with_an_agent_id_is_main_traffic` plus
/// `a_blank_agent_id_is_not_a_child` go red; make it read the class alone and
/// `an_agent_id_without_a_class_is_a_child` goes red, which is the live path
/// on every default shunt deployment.
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    /// The captured parent/child pair from the live-capture note, fact (a).
    #[test]
    fn a_captured_child_turn_parses_as_delegated() {
        let headers = headers(&[
            (SESSION_ID_HEADER, "0f0a2cc3-d5f1-4200-b9c8-f56a081194ce"),
            (AGENT_ID_HEADER, "a7a11c2e22e29e67a"),
            (AGENT_TYPE_HEADER, "Explore"),
            (REQUEST_CLASS_HEADER, "subagent"),
        ]);
        let hints = RouterContext::from_headers(&headers);

        assert!(hints.is_delegated());
        assert_eq!(hints.pin_agent_id(), Some("a7a11c2e22e29e67a"));
        assert_eq!(hints.request_class, Some(RequestClass::Subagent));
        assert_eq!(hints.agent_type, Some("Explore"));
        assert!(!hints.context_compacted);
    }

    #[test]
    fn a_captured_parent_turn_parses_as_main_traffic() {
        let headers = headers(&[
            (SESSION_ID_HEADER, "0f0a2cc3-d5f1-4200-b9c8-f56a081194ce"),
            (REQUEST_CLASS_HEADER, "main"),
        ]);
        let hints = RouterContext::from_headers(&headers);

        assert!(!hints.is_delegated());
        assert_eq!(hints.pin_agent_id(), None);
        assert_eq!(
            hints.session_id,
            Some("0f0a2cc3-d5f1-4200-b9c8-f56a081194ce")
        );
    }

    /// The gate-off shape — every default shunt deployment. The class never
    /// reaches the wire, the agent id still does, and that is enough.
    #[test]
    fn an_agent_id_without_a_class_is_a_child() {
        let headers = headers(&[
            (SESSION_ID_HEADER, "s"),
            (AGENT_ID_HEADER, "acf659811cf929e90"),
        ]);
        let hints = RouterContext::from_headers(&headers);

        assert!(hints.is_delegated());
        assert_eq!(hints.pin_agent_id(), Some("acf659811cf929e90"));
    }

    /// The class is authoritative when sent. Never observed live — no captured
    /// `main` turn carried an agent id — so this pins the defensive branch.
    #[test]
    fn main_with_an_agent_id_is_main_traffic() {
        let headers = headers(&[
            (AGENT_ID_HEADER, "a7a11c2e22e29e67a"),
            (REQUEST_CLASS_HEADER, "main"),
        ]);
        let hints = RouterContext::from_headers(&headers);

        assert!(!hints.is_delegated());
        assert_eq!(
            hints.pin_agent_id(),
            None,
            "main traffic pins in the parent scope"
        );
    }

    #[test]
    fn harness_maintenance_is_not_delegated_even_with_an_agent_id() {
        for class in ["compaction", "auxiliary"] {
            let headers = headers(&[(AGENT_ID_HEADER, "a7a11c2e"), (REQUEST_CLASS_HEADER, class)]);
            assert!(
                !RouterContext::from_headers(&headers).is_delegated(),
                "{class} is harness maintenance, not a child"
            );
        }
    }

    #[test]
    fn a_workflow_turn_is_delegated() {
        let headers = headers(&[(AGENT_ID_HEADER, "w1"), (REQUEST_CLASS_HEADER, "workflow")]);
        assert!(RouterContext::from_headers(&headers).is_delegated());
    }

    /// A class this build does not know is not evidence either way, so the
    /// agent-id rule decides — the same answer the header's absence gives.
    #[test]
    fn an_unrecognised_class_falls_back_to_the_agent_id_rule() {
        let with_id = headers(&[(AGENT_ID_HEADER, "x"), (REQUEST_CLASS_HEADER, "future")]);
        let without = headers(&[(REQUEST_CLASS_HEADER, "future")]);

        assert_eq!(RouterContext::from_headers(&with_id).request_class, None);
        assert!(RouterContext::from_headers(&with_id).is_delegated());
        assert!(!RouterContext::from_headers(&without).is_delegated());
    }

    #[test]
    fn a_blank_agent_id_is_not_a_child() {
        let headers = headers(&[(SESSION_ID_HEADER, "s"), (AGENT_ID_HEADER, "  ")]);
        let hints = RouterContext::from_headers(&headers);

        assert!(!hints.is_delegated());
        assert_eq!(hints.pin_agent_id(), None);
    }

    /// A delegated turn that sent no id has no scope of its own to land in.
    #[test]
    fn a_delegated_class_without_an_agent_id_pins_in_the_parent_scope() {
        let headers = headers(&[(REQUEST_CLASS_HEADER, "subagent")]);
        let hints = RouterContext::from_headers(&headers);

        assert!(hints.is_delegated());
        assert_eq!(hints.pin_agent_id(), None);
    }

    #[test]
    fn context_compacted_is_read_by_presence_not_value() {
        for value in ["manual", "auto", "reactive", "some-future-mode"] {
            let headers = headers(&[(CONTEXT_COMPACTED_HEADER, value)]);
            assert!(
                RouterContext::from_headers(&headers).context_compacted,
                "{value:?} must read as compacted"
            );
        }
        let blank = headers(&[(CONTEXT_COMPACTED_HEADER, "")]);
        assert!(!RouterContext::from_headers(&blank).context_compacted);
        let none = HeaderMap::new();
        assert!(!RouterContext::from_headers(&none).context_compacted);
    }

    #[test]
    fn no_headers_is_the_hint_less_request() {
        let none = HeaderMap::new();
        let hints = RouterContext::from_headers(&none);

        assert_eq!(hints.session_id, None);
        assert!(!hints.is_delegated());
        assert_eq!(hints.pin_agent_id(), None);
        assert!(!hints.context_compacted);
    }
}
