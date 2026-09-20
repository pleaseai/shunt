//! Driven-lane tests, feature-on only.
//!
//! They stop at the algorithm boundary: the stub below stands in for upstream's
//! `PrefillRouterAlgo`, so everything shunt owns — the request it builds, the
//! metadata it derives from the headers, which target it accepts, and what it
//! does when the drive fails — is pinned without a checkpoint, without torch,
//! and without Python on the machine. The real algorithm is upstream's and has
//! its own tests; what this file proves is that shunt hands it the right
//! request and reads the right answer back.
//!
//! Non-vacuity: drop the `Err` arm's fail-open and
//! `a_failed_drive_falls_open_to_the_default_target` panics on `None`; stop
//! filling `metadata.session_id` and
//! `the_session_header_reaches_the_algorithm_as_metadata` goes red.

use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use serde_json::json;
use switchyard_libsy::{Algorithm, Driver, LibsyError, RoutingOutcome};
use switchyard_protocol::{ContentBlock, ModelId, Request, Role, ToolResult};

use super::{decide, messages_from_body, PrefillRouters};
use crate::routing::outcome::RouteSource;

const ROUTER_ID: &str = "claude-prefill";
const TARGETS: [&str; 2] = ["fast-alias", "strong-alias"];

/// Answers with a fixed target (or a fixed failure) and records the request it
/// was handed, modelled on upstream's own `RecordingForward`.
struct StubAlgorithm {
    answer: Result<&'static str, &'static str>,
    seen: Arc<Mutex<Option<Request>>>,
}

#[async_trait::async_trait]
impl Algorithm for StubAlgorithm {
    fn name(&self) -> &str {
        "prefill_router"
    }

    async fn route(
        self: Arc<Self>,
        _driver: Driver,
        request: Request,
    ) -> switchyard_libsy::Result<RoutingOutcome> {
        match self.answer {
            Ok(target) => {
                *self.seen.lock().expect("stub lock") = Some(clone_request(&request));
                Ok(RoutingOutcome::route_to(
                    ModelId::from(target),
                    vec![],
                    request,
                ))
            }
            Err(message) => Err(LibsyError::AlgorithmError {
                message: message.to_string(),
            }),
        }
    }
}

/// `Request` is not `Clone` (its metadata carries a `HeaderMap`), and the test
/// only reads the two correlation fields back, so copy just those.
fn clone_request(request: &Request) -> Request {
    Request {
        llm_request: request.llm_request.clone(),
        raw_request: None,
        metadata: request
            .metadata
            .as_ref()
            .map(|metadata| switchyard_protocol::Metadata {
                session_id: metadata.session_id.clone(),
                agent_id: metadata.agent_id.clone(),
                is_subagent: metadata.is_subagent,
                ..Default::default()
            }),
    }
}

fn routers(
    answer: Result<&'static str, &'static str>,
) -> (PrefillRouters, Arc<Mutex<Option<Request>>>) {
    let seen = Arc::new(Mutex::new(None));
    let algorithm: Arc<dyn Algorithm> = Arc::new(StubAlgorithm {
        answer,
        seen: Arc::clone(&seen),
    });
    (
        PrefillRouters::from_algorithm(ROUTER_ID, &TARGETS, algorithm),
        seen,
    )
}

fn headers(session: Option<&str>, agent: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(session) = session {
        headers.insert("x-claude-code-session-id", session.parse().unwrap());
    }
    if let Some(agent) = agent {
        headers.insert("x-claude-code-agent-id", agent.parse().unwrap());
    }
    headers
}

#[tokio::test]
async fn an_id_that_is_not_a_prefill_entry_is_not_driven() {
    let (routers, seen) = routers(Ok(TARGETS[1]));
    let body = json!({"model": "claude-sonnet-4-6", "messages": []});

    assert!(decide(&routers, &body, &HeaderMap::new()).await.is_none());
    assert!(
        seen.lock().unwrap().is_none(),
        "an unrelated id must not reach the algorithm at all"
    );
}

#[tokio::test]
async fn a_driven_decision_names_the_selected_target() {
    let (routers, _) = routers(Ok(TARGETS[1]));
    let body = json!({"model": ROUTER_ID, "messages": [{"role": "user", "content": "hi"}]});

    let decision = decide(&routers, &body, &HeaderMap::new())
        .await
        .expect("a prefill entry is driven");

    assert_eq!(decision.target, TARGETS[1]);
    assert_eq!(decision.source.as_label(), "prefill");
}

/// The hint is stripped before the lookup, exactly as `resolve_chain` strips
/// it: a `[1m]` request that missed here would silently take the default.
#[tokio::test]
async fn the_context_window_hint_is_stripped_before_the_lookup() {
    let (routers, _) = routers(Ok(TARGETS[1]));
    let body = json!({"model": format!("{ROUTER_ID}[1m]"), "messages": []});

    let decision = decide(&routers, &body, &HeaderMap::new())
        .await
        .expect("the bare id is found");

    assert_eq!(decision.target, TARGETS[1]);
}

/// libsy's affinity keys on these two fields, so a decision only survives a
/// tool continuation if they actually arrive.
#[tokio::test]
async fn the_session_header_reaches_the_algorithm_as_metadata() {
    let (routers, seen) = routers(Ok(TARGETS[0]));
    let body = json!({"model": ROUTER_ID, "messages": []});

    decide(
        &routers,
        &body,
        &headers(Some(" session-a "), Some("agent-b")),
    )
    .await
    .expect("a prefill entry is driven");

    let request = seen.lock().unwrap().take().expect("the stub saw a request");
    let metadata = request.metadata.expect("metadata is attached");
    assert_eq!(metadata.session_id.as_deref(), Some("session-a"));
    assert_eq!(metadata.agent_id.as_deref(), Some("agent-b"));
    assert!(metadata.is_subagent, "an agent id marks a delegated turn");
}

/// The class header outranks the agent id, exactly as it does for the stage
/// router's pins. A `main` turn that carries an agent id is root traffic: key
/// it as a child and its decision lands in a scope the parent's own
/// continuation never reads, splitting one conversation across two affinity
/// entries.
///
/// Non-vacuity: restore `is_subagent: agent_id.is_some()` in
/// `metadata_from_headers` and both halves of this test go red.
#[tokio::test]
async fn the_request_class_outranks_the_agent_id() {
    let (routers, seen) = routers(Ok(TARGETS[0]));
    let body = json!({"model": ROUTER_ID, "messages": []});

    let mut main_with_an_agent_id = headers(Some("session-a"), Some("agent-b"));
    main_with_an_agent_id.insert("x-claude-code-request-class", "main".parse().unwrap());
    decide(&routers, &body, &main_with_an_agent_id)
        .await
        .expect("a prefill entry is driven");

    let request = seen.lock().unwrap().take().expect("the stub saw a request");
    let metadata = request.metadata.expect("metadata is attached");
    assert!(
        !metadata.is_subagent,
        "a `main` turn is root traffic even when it carries an agent id"
    );
    assert_eq!(
        metadata.agent_id, None,
        "a root turn is keyed on its session alone"
    );

    let mut delegated_without_an_id = headers(Some("session-a"), None);
    delegated_without_an_id.insert("x-claude-code-request-class", "subagent".parse().unwrap());
    decide(&routers, &body, &delegated_without_an_id)
        .await
        .expect("a prefill entry is driven");

    let request = seen.lock().unwrap().take().expect("the stub saw a request");
    let metadata = request.metadata.expect("metadata is attached");
    assert!(
        metadata.is_subagent,
        "a `subagent` turn is delegated whether or not it sent an agent id"
    );
}

#[tokio::test]
async fn a_blank_session_header_is_absent_rather_than_a_shared_key() {
    let (routers, seen) = routers(Ok(TARGETS[0]));
    let body = json!({"model": ROUTER_ID, "messages": []});

    decide(&routers, &body, &headers(Some("   "), None))
        .await
        .expect("a prefill entry is driven");

    let request = seen.lock().unwrap().take().expect("the stub saw a request");
    let metadata = request.metadata.expect("metadata is attached");
    assert_eq!(metadata.session_id, None);
    assert!(!metadata.is_subagent);
}

/// Learned routing is an optimization: a checkpoint that cannot answer must
/// not take the conversation down with it.
#[tokio::test]
async fn a_failed_drive_falls_open_to_the_default_target() {
    let (routers, _) = routers(Err("checkpoint exploded"));
    let body = json!({"model": ROUTER_ID, "messages": []});

    let decision = decide(&routers, &body, &HeaderMap::new())
        .await
        .expect("a failed drive still decides");

    assert_eq!(
        decision.target, TARGETS[0],
        "the first target is the default"
    );
    assert_eq!(decision.source.as_label(), "prefill_fail_open");
    assert!(matches!(decision.source, RouteSource::PrefillFailOpen));
}

#[test]
fn a_string_content_becomes_one_text_block() {
    let messages = messages_from_body(&json!([{"role": "user", "content": "write a test"}]));

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(
        messages[0].content,
        vec![ContentBlock::Text {
            text: "write a test".to_string()
        }]
    );
}

/// The two block kinds the upstream algorithm reads survive; everything else
/// is dropped, because neither its scorer nor its affinity looks at them.
#[test]
fn only_text_and_tool_result_blocks_survive() {
    let messages = messages_from_body(&json!([
        {"role": "assistant", "content": [
            {"type": "tool_use", "id": "a", "name": "Read"},
            {"type": "text", "text": "reading"},
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "a", "is_error": true},
            {"type": "image", "source": {"type": "url", "url": "http://example"}},
        ]},
        {"role": "system", "content": "ignored"},
    ]));

    assert_eq!(messages.len(), 2, "the system turn is skipped");
    assert_eq!(messages[0].role, Role::Assistant);
    assert_eq!(
        messages[0].content,
        vec![ContentBlock::Text {
            text: "reading".to_string()
        }],
        "the tool_use block is dropped"
    );
    assert_eq!(
        messages[1].content,
        vec![ContentBlock::ToolResult(ToolResult {
            tool_call_id: "a".to_string(),
            content: Vec::new(),
            is_error: Some(true),
        })],
        "the image block is dropped and the tool result keeps its error flag"
    );
}

#[test]
fn a_tool_result_without_an_error_flag_reports_none() {
    let messages = messages_from_body(&json!([
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "b"}]},
    ]));

    assert_eq!(
        messages[0].content,
        vec![ContentBlock::ToolResult(ToolResult {
            tool_call_id: "b".to_string(),
            content: Vec::new(),
            is_error: None,
        })]
    );
}

#[test]
fn a_body_without_a_messages_array_yields_no_messages() {
    assert!(messages_from_body(&json!(null)).is_empty());
    assert!(messages_from_body(&json!("not an array")).is_empty());
}
