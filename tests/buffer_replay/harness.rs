//! The deployment `tests/buffer_replay.rs` runs against, and the upstream
//! reply shapes its gated turns are made of.
//!
//! Four upstreams: an injecting strong tier, a passthrough Anthropic weak or
//! executor tier (the gated turn is the caller's own dispatch, so it may be
//! passthrough), an injecting OpenAI Responses tier — the second adapter a
//! retained turn must render correctly — and an injecting judge. The judge's
//! URL is a parameter rather than a mock so the stall tests can put a raw
//! socket there.

use serde_json::{json, Value};
use shunt::config::{
    ApiKeyHeader, AuthMap, AuthMode, Config, InboundAuthConfig, ModelConfig, ProviderKind,
    RouterConfig, UpstreamAuth, UpstreamConfig,
};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::judge_harness::{
    alias, api_key, client, upstream_with, TestGateway, CAPABLE_KEY_ENV, CAPABLE_UPSTREAM_MODEL,
    CLIENT_TOKEN, EFFICIENT_UPSTREAM_MODEL, JUDGE_KEY_ENV, JUDGE_UPSTREAM_MODEL, ROUTER_ID,
    SESSION, TOKENS_ENV,
};

pub(crate) const RESPONSES_UPSTREAM_MODEL: &str = "upstream-responses";

/// Where each tier lives.
pub(crate) struct Tiers {
    pub(crate) strong: String,
    pub(crate) weak: String,
    pub(crate) responses: String,
    pub(crate) judge: String,
}

impl Tiers {
    pub(crate) fn of(
        strong: &MockServer,
        weak: &MockServer,
        responses: &MockServer,
        judge: String,
    ) -> Self {
        Self {
            strong: strong.uri(),
            weak: weak.uri(),
            responses: responses.uri(),
            judge,
        }
    }
}

/// The gated entry under `ROUTER_ID`, with `router` as its `[models.router]`.
pub(crate) fn gated_config(tiers: &Tiers, router: &str) -> Config {
    let mut config = Config::default();
    config.providers.clear();
    let mut responses = api_key("responses", tiers.responses.clone(), JUDGE_KEY_ENV);
    responses.kind = Some(ProviderKind::Responses);
    config.upstreams = vec![
        api_key("capable", tiers.strong.clone(), CAPABLE_KEY_ENV),
        upstream_with(
            "efficient",
            tiers.weak.clone(),
            UpstreamAuth::Shorthand(AuthMode::Passthrough),
        ),
        responses,
        judge_upstream(tiers.judge.clone()),
    ];
    config.server.default_provider = "efficient".to_string();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: TOKENS_ENV.to_string(),
    });
    config.models = vec![
        ModelConfig {
            subagents: None,
            id: ROUTER_ID.to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(toml::from_str::<RouterConfig>(router).expect("the router table parses")),
            stage_router: None,
        },
        alias("capable-alias", "capable", CAPABLE_UPSTREAM_MODEL),
        alias("efficient-alias", "efficient", EFFICIENT_UPSTREAM_MODEL),
        alias("responses-alias", "responses", RESPONSES_UPSTREAM_MODEL),
        alias("judge-alias", "judge", JUDGE_UPSTREAM_MODEL),
    ];
    config.validate().expect("the gated config is well formed")
}

fn judge_upstream(url: String) -> UpstreamConfig {
    upstream_with(
        "judge",
        url,
        UpstreamAuth::Map(AuthMap::ApiKey {
            env: Some(JUDGE_KEY_ENV.to_string()),
            header: Some(ApiKeyHeader::XApiKey),
        }),
    )
}

/// An escalation entry whose weak tier is `weak`, with deadlines short enough
/// for the stall tests.
pub(crate) fn escalation_router(weak: &str) -> String {
    format!(
        r#"
type = "llm_classifier"
mode = "escalation"
classifier_target = "judge-alias"
strong_target = "capable-alias"
weak_target = "{weak}"
judge_timeout_ms = 300
"#
    )
}

/// An advisor entry on the passthrough Anthropic executor.
pub(crate) const ADVISOR_ROUTER: &str = r#"
type = "advisor"
executor_target = "efficient-alias"
advisor_target = "judge-alias"
judge_timeout_ms = 300
"#;

/// One gated request, in the mode the test names.
pub(crate) async fn post(gateway: &TestGateway, stream: bool) -> reqwest::Response {
    post_in_session(gateway, stream, Some(SESSION)).await
}

/// [`post`], with the session header set to `session` or left off entirely.
pub(crate) async fn post_in_session(
    gateway: &TestGateway,
    stream: bool,
    session: Option<&str>,
) -> reqwest::Response {
    let mut request = client()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-shunt-token", CLIENT_TOKEN);
    if let Some(session) = session {
        request = request.header("x-claude-code-session-id", session);
    }
    request
        .body(
            json!({
                "model": ROUTER_ID,
                "max_tokens": 64,
                "stream": stream,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "add a --json flag"}]}],
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap()
}

pub(crate) fn header<'a>(response: &'a reqwest::Response, name: &str) -> &'a str {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("{name} is stamped"))
        .to_str()
        .expect("the header is ASCII")
}

/// A complete Anthropic SSE turn under the upstream's own model id — which
/// the adapter must rewrite to the router id before the turn is retained.
pub(crate) fn anthropic_sse(upstream_model: &str, text: &str) -> String {
    anthropic_frames(upstream_model, text, true)
}

/// The same turn cut before `message_delta` and `message_stop`: a `200` whose
/// body simply ends.
pub(crate) fn anthropic_sse_truncated(upstream_model: &str, text: &str) -> String {
    anthropic_frames(upstream_model, text, false)
}

fn anthropic_frames(upstream_model: &str, text: &str, complete: bool) -> String {
    let mut frames = vec![
        (
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_gated", "type": "message", "role": "assistant",
                "model": upstream_model, "content": [], "stop_reason": null,
                "usage": {"input_tokens": 5, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": text}}),
        ),
    ];
    if complete {
        frames.extend([
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 3}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ]);
    }
    frames
        .into_iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect()
}

pub(crate) fn sse_reply(body: String) -> ResponseTemplate {
    // `set_body_string` would stamp `text/plain` over an inserted header.
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// A whole non-streaming Anthropic message.
pub(crate) fn anthropic_json(upstream_model: &str, text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_gated",
        "type": "message",
        "role": "assistant",
        "model": upstream_model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3},
    }))
}

/// A Responses turn in the SSE shape the Responses adapter translates — it
/// always streams upstream, whatever mode the client asked for.
pub(crate) fn responses_sse(text: &str) -> ResponseTemplate {
    let body = format!(
        concat!(
            "event: response.created\n",
            "data: {{\"response\":{{\"id\":\"resp_gated\",\"usage\":{{\"output_tokens\":0}}}}}}\n\n",
            "event: response.output_text.delta\n",
            "data: {{\"delta\":{delta}}}\n\n",
            "event: response.output_text.done\n",
            "data: {{}}\n\n",
            "event: response.completed\n",
            "data: {{\"response\":{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":3}}}}}}\n\n",
            "data: [DONE]\n\n"
        ),
        delta = json!(text)
    );
    sse_reply(body)
}

/// A judge or advisor answering `text` in the captured Anthropic shape.
pub(crate) fn judge_text(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_judge",
        "type": "message",
        "role": "assistant",
        "model": JUDGE_UPSTREAM_MODEL,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    }))
}

pub(crate) fn messages_mock(reply: ResponseTemplate, expect: u64) -> Mock {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(reply)
        .expect(expect)
}

/// Every `(event, data)` pair of an SSE body, in order.
pub(crate) fn sse_events(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter_map(|frame| {
            let event = frame
                .lines()
                .find_map(|line| line.strip_prefix("event:"))?
                .trim()
                .to_string();
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data:"))
                .and_then(|data| serde_json::from_str(data.trim()).ok())
                .unwrap_or(Value::Null);
            Some((event, data))
        })
        .collect()
}

/// The text a streamed turn carried, concatenated.
pub(crate) fn streamed_text(events: &[(String, Value)]) -> String {
    events
        .iter()
        .filter_map(|(_, data)| data.pointer("/delta/text").and_then(Value::as_str))
        .collect()
}

/// The request bodies a mock received, in order.
pub(crate) async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("the mock server records requests")
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("a JSON body"))
        .collect()
}
