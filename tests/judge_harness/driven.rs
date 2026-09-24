//! Builders for `tests/driven_lane.rs`: the four driven shapes ADR-0005 §8
//! PR 5 adds, on the same three-upstream deployment
//! [`super::driven_config`] uses for the stage-router judge.
//!
//! The upstream mix is deliberate and shared with that harness — an injecting
//! capable tier, a passthrough efficient tier, and an injecting judge — because
//! it is what makes the admission envelope observable: a turn that will consult
//! is gated on the judge's credential even when the tier it lands on needs
//! none.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use shunt::config::{
    ApiKeyHeader, AuthMap, AuthMode, Config, CountTokens, InboundAuthConfig, ModelConfig,
    ProviderKind, RetryConfig, RouterConfig, SubagentsConfig, UpstreamAuth, UpstreamConfig,
};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::{
    alias, api_key, upstream_with, CAPABLE_KEY_ENV, CAPABLE_UPSTREAM_MODEL, CLIENT_TOKEN,
    EFFICIENT_UPSTREAM_MODEL, JUDGE_KEY_ENV, ROUTER_ID, TOKENS_ENV,
};

/// The parent destination of an overlay entry: where a turn the overlay does
/// **not** divert lands.
pub(crate) const PARENT_UPSTREAM_MODEL: &str = "upstream-parent";

/// One judge upstream, with the alias a router table names it by.
pub(crate) struct Judge {
    alias: &'static str,
    upstream_model: &'static str,
    url: String,
    kind: ProviderKind,
}

impl Judge {
    /// An Anthropic-protocol judge — the shape the live capture was taken
    /// against, and what every judge target is unless it is a Responses one.
    pub(crate) fn anthropic(alias: &'static str, server: &MockServer) -> Self {
        Self {
            alias,
            upstream_model: upstream_model_for(alias),
            url: server.uri(),
            kind: ProviderKind::Anthropic,
        }
    }

    /// A judge on an OpenAI Responses provider: the path `output_config.format`
    /// has to survive as `text.format` on.
    pub(crate) fn responses(alias: &'static str, server: &MockServer) -> Self {
        Self {
            alias,
            upstream_model: upstream_model_for(alias),
            url: server.uri(),
            kind: ProviderKind::Responses,
        }
    }
}

/// `judge-a` → `upstream-judge-a`, so a mock can match on the body's `model`.
fn upstream_model_for(alias: &'static str) -> &'static str {
    match alias {
        "judge-a" => "upstream-judge-a",
        "judge-b" => "upstream-judge-b",
        other => panic!("unknown judge alias {other}"),
    }
}

/// What the requested `[[models]]` entry carries.
pub(crate) enum Entry<'a> {
    /// A `[models.router]` table, as TOML.
    Router(&'a str),
    /// A `[models.subagents]` table, as TOML. The entry also maps to the
    /// passthrough upstream, so a turn the overlay does not divert has a
    /// parent destination to land on.
    Overlay(&'a str),
}

/// The deployment every test in `tests/driven_lane.rs` runs against.
pub(crate) fn lane_config(
    capable: &MockServer,
    efficient: &MockServer,
    judges: &[Judge],
    entry: Entry<'_>,
) -> Config {
    let mut config = Config::default();
    config.providers.clear();
    config.upstreams = vec![
        api_key("capable", capable.uri(), CAPABLE_KEY_ENV),
        upstream_with(
            "efficient",
            efficient.uri(),
            UpstreamAuth::Shorthand(AuthMode::Passthrough),
        ),
    ];
    config.server.default_provider = "efficient".to_string();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: TOKENS_ENV.to_string(),
    });

    let mut models = vec![
        alias("capable-alias", "capable", CAPABLE_UPSTREAM_MODEL),
        alias("efficient-alias", "efficient", EFFICIENT_UPSTREAM_MODEL),
    ];
    for judge in judges {
        config
            .upstreams
            .push(judge_upstream(judge.alias, judge.url.clone(), judge.kind));
        models.push(alias(judge.alias, judge.alias, judge.upstream_model));
    }

    let host = match entry {
        Entry::Router(toml) => ModelConfig {
            subagents: None,
            id: ROUTER_ID.to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(toml::from_str::<RouterConfig>(toml).expect("the router table parses")),
            stage_router: None,
        },
        Entry::Overlay(toml) => ModelConfig {
            subagents: Some(
                toml::from_str::<SubagentsConfig>(toml).expect("the overlay table parses"),
            ),
            id: ROUTER_ID.to_string(),
            display_name: None,
            upstream_model: Some(BTreeMap::from([(
                "efficient".to_string(),
                PARENT_UPSTREAM_MODEL.to_string(),
            )])),
            router: None,
            stage_router: None,
        },
    };
    models.insert(0, host);
    config.models = models;
    config.validate().expect("the driven config is well formed")
}

/// A judge upstream. Api-key auth on both protocols, because a judge target
/// must be credential-injecting — validation refuses a passthrough one.
fn judge_upstream(name: &str, base_url: String, kind: ProviderKind) -> UpstreamConfig {
    UpstreamConfig {
        name: name.to_string(),
        provider: None,
        kind: Some(kind),
        base_url: Some(base_url),
        auth: Some(UpstreamAuth::Map(AuthMap::ApiKey {
            env: Some(JUDGE_KEY_ENV.to_string()),
            header: Some(ApiKeyHeader::XApiKey),
        })),
        effort: None,
        service_tier: None,
        classifier_model: None,
        count_tokens: CountTokens::Tiktoken,
        websocket: false,
        tool_search: None,
        request_compression: true,
        retry: RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        workspace_roots: Vec::new(),
        profile_dir: None,
        sandbox: true,
    }
}

/// `mode = "capability"` over the two tiers, with the short judge deadline the
/// rest of this harness uses.
pub(crate) const CAPABILITY_ROUTER: &str = r#"
type = "llm_classifier"
mode = "capability"
classifier_target = "judge-a"
strong_target = "capable-alias"
weak_target = "efficient-alias"
base_threshold = 0.5
judge_timeout_ms = 500
"#;

/// The same, budgeted to a single call per `(session, agent)`.
pub(crate) const CAPABILITY_ROUTER_ONE_CALL: &str = r#"
type = "llm_classifier"
mode = "capability"
classifier_target = "judge-a"
strong_target = "capable-alias"
weak_target = "efficient-alias"
base_threshold = 0.5
judge_timeout_ms = 500
max_judge_calls = 1
"#;

/// `mode = "custom"` with two judge candidates, so a failing first judge is
/// observably *not* followed by a second call.
pub(crate) const CUSTOM_ROUTER_TWO_JUDGES: &str = r#"
type = "llm_classifier"
mode = "custom"
default_target = "efficient"
prompt = "Reply with the group that should serve this turn."
response_schema = '{"type":"object","properties":{"target":{"type":"string","enum":["capable","efficient"]}},"required":["target"],"additionalProperties":false}'
policy = { type = "target_selector", selector = "/target" }
models = { judge = ["judge-a", "judge-b"], capable = ["capable-alias"], efficient = ["efficient-alias"], any = ["capable-alias", "efficient-alias"] }
judge_timeout_ms = 500
"#;

/// A single-judge custom entry, for the Responses judge path.
pub(crate) const CUSTOM_ROUTER: &str = r#"
type = "llm_classifier"
mode = "custom"
default_target = "efficient"
prompt = "Reply with the group that should serve this turn."
response_schema = '{"type":"object","properties":{"target":{"type":"string","enum":["capable","efficient"]}},"required":["target"],"additionalProperties":false}'
policy = { type = "target_selector", selector = "/target" }
models = { judge = ["judge-a"], capable = ["capable-alias"], efficient = ["efficient-alias"], any = ["capable-alias", "efficient-alias"] }
judge_timeout_ms = 500
"#;

/// `type = "composite"` on `user_turn`: the judge sets the tier a human turn
/// opens and the scorer serves the tool continuations that follow.
pub(crate) const COMPOSITE_ROUTER: &str = r#"
type = "composite"
judge_timeout_ms = 500

[classifier]
target = "judge-a"
base_threshold = 0.5
classify_trigger = "user_turn"

[stage]
capable_target = "capable-alias"
efficient_target = "efficient-alias"
confidence_threshold = 0.5
"#;

/// The same composite, budgeted to a single call per `(session, agent)`.
///
/// The composite is one of the two forms that can put a judge ahead of another
/// decision, so `max_judge_calls` has to bind here and not only on the plain
/// classifier entry.
pub(crate) const COMPOSITE_ROUTER_ONE_CALL: &str = r#"
type = "composite"
judge_timeout_ms = 500
max_judge_calls = 1

[classifier]
target = "judge-a"
base_threshold = 0.5
classify_trigger = "user_turn"

[stage]
capable_target = "capable-alias"
efficient_target = "efficient-alias"
confidence_threshold = 0.5
"#;

/// The overlay, budgeted to a single call and asked to classify every turn.
///
/// `new_session` would cap the overlay at one judge call per `(session, agent)`
/// on its own, which would make a `max_judge_calls` test vacuous — the trigger,
/// not the budget, would be the thing refusing the second call. `every_request`
/// is what leaves the budget as the only bound in play. It is an accepted
/// trigger for this table; only `user_turn` is refused for a sub-agent overlay.
pub(crate) const CLASSIFIER_OVERLAY_ONE_CALL: &str = r#"
type = "llm_classifier"
mode = "custom"
default_target = "efficient"
prompt = "Reply with the group that should serve this delegated turn."
response_schema = '{"type":"object","properties":{"target":{"type":"string","enum":["capable","efficient"]}},"required":["target"],"additionalProperties":false}'
classify_trigger = "every_request"
policy = { type = "target_selector", selector = "/target" }
models = { judge = ["judge-a"], capable = ["capable-alias"], efficient = ["efficient-alias"], any = ["capable-alias", "efficient-alias"] }
judge_timeout_ms = 500
max_judge_calls = 1
"#;

/// The classifier form of `[models.subagents]`: one classification per
/// `(session, agent)`, which is what `classify_trigger = "new_session"` means
/// for an overlay.
pub(crate) const CLASSIFIER_OVERLAY: &str = r#"
type = "llm_classifier"
mode = "custom"
default_target = "efficient"
prompt = "Reply with the group that should serve this delegated turn."
response_schema = '{"type":"object","properties":{"target":{"type":"string","enum":["capable","efficient"]}},"required":["target"],"additionalProperties":false}'
policy = { type = "target_selector", selector = "/target" }
models = { judge = ["judge-a"], capable = ["capable-alias"], efficient = ["efficient-alias"], any = ["capable-alias", "efficient-alias"] }
judge_timeout_ms = 500
"#;

/// The captured Anthropic reply shape
/// (`docs/notes/adr-0005-routing-live-captures.md`, "Fact (c), re-captured"):
/// `stop_reason: end_turn` and a `content` array of exactly one `text` block
/// whose text is the verdict JSON — no preamble, no fence.
pub(crate) fn captured_capability_reply(upstream_model: &str, p_solve: f64) -> ResponseTemplate {
    let verdict = json!({
        "crux": "Implement a --json flag for the report subcommand that produces output matching both the text renderer's rows and the exact object shape defined in the test file tests/test_report_json.py, verified by passing pytest",
        "primary_rule": "SUP-1",
        "capability_boundary": "supported",
        "p_solve": p_solve,
    });
    anthropic_text_reply(upstream_model, &verdict.to_string())
}

/// The same shape carrying a custom classifier's verdict.
pub(crate) fn captured_custom_reply(upstream_model: &str, target: &str) -> ResponseTemplate {
    anthropic_text_reply(upstream_model, &json!({"target": target}).to_string())
}

fn anthropic_text_reply(upstream_model: &str, text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(
        json!({
            "id": "msg_judge",
            "type": "message",
            "role": "assistant",
            "model": upstream_model,
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1_200, "output_tokens": 91},
        })
        .to_string(),
    )
}

/// The captured `400`, verbatim
/// (`docs/notes/adr-0005-routing-live-captures.md`, "What a rejection of the
/// field itself looks like"). A schema keyword Anthropic's grammar refuses, on
/// the *first* judge call — which is exactly the failure the classifier's own
/// fallback has to absorb.
pub(crate) const CAPTURED_SCHEMA_REJECTION: &str = concat!(
    r#"{"type": "error", "#,
    r#""error": {"type": "invalid_request_error", "#,
    r#""message": "output_config.format.schema: For 'number' type, properties maximum, minimum are not supported"}, "#,
    r#""request_id": "req_011CfCFX7pQ534gNu2T3Tj1Y"}"#
);

/// A Responses-protocol judge reply, in the SSE shape
/// `tests/auto_mode_safeguards.rs` captures.
pub(crate) fn responses_verdict_sse(target: &str) -> ResponseTemplate {
    let delta = json!({"target": target}).to_string();
    let body = format!(
        concat!(
            "event: response.created\n",
            "data: {{\"response\":{{\"id\":\"resp_judge\",\"usage\":{{\"output_tokens\":0}}}}}}\n\n",
            "event: response.output_text.delta\n",
            "data: {{\"delta\":{delta}}}\n\n",
            "event: response.output_text.done\n",
            "data: {{}}\n\n",
            "event: response.completed\n",
            "data: {{\"response\":{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":9}}}}}}\n\n",
            "data: [DONE]\n\n"
        ),
        delta = json!(delta)
    );
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// A judge mock that answers every `/v1/messages` with `reply`.
pub(crate) fn judge_reply_mock(reply: ResponseTemplate, expect: u64) -> Mock {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(reply)
        .expect(expect)
}

/// A judge mock on the Responses path.
pub(crate) fn responses_judge_mock(reply: ResponseTemplate, expect: u64) -> Mock {
    Mock::given(method("POST"))
        .respond_with(reply)
        .expect(expect)
}

/// The single request body a mock server received, for the assertions that are
/// about what shunt *sent* rather than where the turn landed.
pub(crate) async fn only_request_body(server: &MockServer) -> Value {
    let requests = server
        .received_requests()
        .await
        .expect("the mock server records requests");
    assert_eq!(requests.len(), 1, "expected exactly one upstream request");
    serde_json::from_slice(&requests[0].body).expect("the upstream body is JSON")
}

/// Whether `key` occurs anywhere in `value`, at any depth.
pub(crate) fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|nested| contains_key(nested, key))
        }
        Value::Array(items) => items.iter().any(|item| contains_key(item, key)),
        _ => false,
    }
}

/// The client token every test in the driven-lane binary presents.
pub(crate) const TOKEN: &str = CLIENT_TOKEN;
