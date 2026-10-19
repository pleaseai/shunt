//! Non-vacuity. The wrap has two failure modes that look alike from one side:
//! wrapping nothing (the leak of issue #783 stays) and wrapping everything
//! (Anthropic's own signatures stop round-tripping). Every assertion that a
//! response *is* wrapped is paired with one that a first-party or
//! `claude_oauth` response is *not*, so forcing [`signing_key`] either way
//! turns one of them red. The request side is paired the same way: an
//! envelope restored for its own upstream, and one left for the strip
//! everywhere else.

use axum::body::Bytes;
use futures_util::{stream, FutureExt, StreamExt};
use serde_json::{json, Value};

use super::*;
use crate::config::{AuthMode, Config, ProviderConfig};
use crate::model::thinking_signature::{unwrap_upstream_signature, wrap_upstream_signature};

const THIRD_PARTY: &str = "https://api.minimax.cn/anthropic";

fn provider(base_url: &str, auth: AuthMode) -> ProviderConfig {
    let mut provider = Config::default()
        .provider("anthropic")
        .expect("built-in anthropic provider")
        .clone();
    provider.base_url = base_url.to_string();
    provider.auth = auth;
    provider
}

#[test]
fn only_a_third_party_provider_has_a_signing_key() {
    assert_eq!(
        signing_key(&provider(&format!("{THIRD_PARTY}/"), AuthMode::ApiKey)),
        Some(THIRD_PARTY),
        "a third-party host signs its own thinking, keyed on the trimmed base_url"
    );
    assert_eq!(
        signing_key(&provider(
            "https://api.anthropic.com",
            AuthMode::Passthrough
        )),
        None,
        "api.anthropic.com is the host the strip exists to protect"
    );
    assert_eq!(
        signing_key(&provider(THIRD_PARTY, AuthMode::ClaudeOauth)),
        None,
        "a claude_oauth pool is Anthropic whatever base_url it is pointed at"
    );
}

// ---------------------------------------------------------------- request

fn body(request: Value) -> RequestBody {
    RequestBody::parse(serde_json::to_vec(&request).unwrap()).unwrap()
}

fn transcript(signature: &str) -> RequestBody {
    body(json!({
        "model": "MiniMax-M3",
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "weighing it", "signature": signature},
                {"type": "text", "text": "done"},
            ]},
        ],
    }))
}

fn first_block(request: &RequestBody) -> Value {
    request.json()["messages"][1]["content"][0].clone()
}

/// Restore, then strip — the order both non-`claude_oauth` paths run them in.
fn outbound(mut request: RequestBody, to: &ProviderConfig) -> RequestBody {
    restore_own_signatures(&mut request, to);
    super::super::thinking::strip_foreign_thinking(&mut request);
    request
}

#[test]
fn an_envelope_going_back_to_its_upstream_carries_the_original_signature() {
    let wrapped = wrap_upstream_signature(THIRD_PARTY, "mm-sig+/=");
    let request = outbound(
        transcript(&wrapped),
        &provider(THIRD_PARTY, AuthMode::ApiKey),
    );
    let block = first_block(&request);
    assert_eq!(block["type"], "thinking");
    assert_eq!(
        block["signature"], "mm-sig+/=",
        "the host that issued the signature gets it back byte for byte"
    );
}

#[test]
fn an_envelope_going_anywhere_else_is_dropped_by_the_strip() {
    let wrapped = wrap_upstream_signature(THIRD_PARTY, "mm-sig");
    for (name, to) in [
        (
            "another third-party host",
            provider("https://open.bigmodel.cn/api/anthropic", AuthMode::ApiKey),
        ),
        (
            "api.anthropic.com",
            provider("https://api.anthropic.com", AuthMode::Passthrough),
        ),
        (
            "a claude_oauth pool",
            provider(THIRD_PARTY, AuthMode::ClaudeOauth),
        ),
    ] {
        let request = outbound(transcript(&wrapped), &to);
        assert_eq!(
            first_block(&request)["type"],
            "text",
            "{name} cannot verify MiniMax's signature, so the block must not reach it"
        );
    }
}

#[test]
fn a_genuine_signature_is_untouched_and_the_body_keeps_its_bytes() {
    // Spacing `serde_json::to_vec` would not reproduce, so a needless
    // re-serialisation by the restore is visible rather than coincidentally equal.
    let raw = br#"{ "messages": [{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"ErUBCkYIBBgCKkB"}]}] }"#;
    let mut request = RequestBody::parse(raw.to_vec()).unwrap();
    restore_own_signatures(&mut request, &provider(THIRD_PARTY, AuthMode::ApiKey));
    assert_eq!(request.into_raw(), raw.to_vec());
}

// ------------------------------------------------------------ non-streaming

#[test]
fn a_buffered_response_has_each_thinking_signature_wrapped() {
    let raw = json!({
        "model": "MiniMax-M3",
        "content": [
            {"type": "thinking", "thinking": "a", "signature": "mm-sig"},
            {"type": "thinking", "thinking": "b", "signature": ""},
            {"type": "text", "text": "done"},
        ],
    });
    let out = wrap_response_signatures(Bytes::from(raw.to_string()), THIRD_PARTY);
    let value: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        unwrap_upstream_signature(value["content"][0]["signature"].as_str().unwrap()),
        Some((THIRD_PARTY.to_string(), "mm-sig".to_string()))
    );
    assert_eq!(
        value["content"][1]["signature"], "",
        "an empty signature is no host's and stays as it is"
    );
    assert_eq!(value["content"][2]["text"], "done");
}

#[test]
fn a_buffered_response_without_a_signature_keeps_its_bytes() {
    let raw = Bytes::from_static(br#"{ "content": [{"type":"text","text":"hi"}] }"#);
    assert_eq!(wrap_response_signatures(raw.clone(), THIRD_PARTY), raw);
}

// ---------------------------------------------------------------- streaming

type Item = Result<Bytes, std::convert::Infallible>;

fn chunk(text: &str) -> Item {
    Ok(Bytes::from(text.to_owned()))
}

async fn collect(items: Vec<Item>, key: Option<&str>) -> String {
    collect_within(items, key, 64 * 1024).await
}

async fn collect_within(items: Vec<Item>, key: Option<&str>, max_frame: usize) -> String {
    wrap_signature_stream(stream::iter(items), key.map(str::to_owned), max_frame)
        .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
        .collect()
        .await
}

const TEXT_DELTA: &str = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";

fn signature_deltas(out: &str) -> Vec<(u64, String)> {
    out.split("\n\n")
        .filter_map(super::frame_data)
        .filter(|event| event["delta"]["type"] == "signature_delta")
        .map(|event| {
            (
                event["index"].as_u64().unwrap(),
                event["delta"]["signature"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The shape a client concatenates: a start carrying part of the signature
/// and several `signature_delta` fragments, with frames and even the frame
/// boundary itself split across chunks.
#[tokio::test]
async fn a_streamed_signature_leaves_as_one_wrapped_delta_before_its_stop() {
    let out = collect(
        vec![
            chunk("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"AA\"}}\n\n"),
            chunk("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_"),
            chunk("delta\",\"signature\":\"BB\"}}\n"),
            chunk("\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"CC\"}}\n\n"),
            chunk("event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n"),
            chunk(TEXT_DELTA),
        ],
        Some(THIRD_PARTY),
    )
    .await;

    let deltas = signature_deltas(&out);
    assert_eq!(
        deltas.len(),
        1,
        "one signature_delta, not one per fragment: {out}"
    );
    assert_eq!(
        unwrap_upstream_signature(&deltas[0].1),
        Some((THIRD_PARTY.to_string(), "AABBCC".to_string())),
        "the start's part and every fragment, in order, under one envelope"
    );
    let delta_at = out.find("signature_delta").unwrap();
    let stop_at = out.find("content_block_stop").unwrap();
    assert!(
        delta_at < stop_at && !out[delta_at..stop_at].contains("content_block_delta\ndata"),
        "the wrapped delta sits immediately before the block's stop"
    );
    assert!(
        out.contains("\"signature\":\"\"}}"),
        "the start's own signature is blanked, so the client's concatenation is the envelope alone"
    );
    assert!(out.contains("\"thinking\":\"hmm\""));
    assert!(
        out.ends_with(TEXT_DELTA),
        "a text frame passes byte-identical"
    );
}

#[tokio::test]
async fn a_text_frame_is_forwarded_as_soon_as_it_completes() {
    // The upstream then goes quiet forever: the frame must not be held for
    // anything that follows it.
    let upstream = stream::iter(vec![chunk(TEXT_DELTA)]).chain(stream::pending());
    let mut wrapped = Box::pin(wrap_signature_stream(
        upstream,
        Some(THIRD_PARTY.to_string()),
        64 * 1024,
    ));
    let first = wrapped
        .next()
        .now_or_never()
        .expect("the completed frame is ready without waiting on the upstream")
        .expect("an item")
        .unwrap();
    assert_eq!(first, Bytes::from_static(TEXT_DELTA.as_bytes()));
}

#[tokio::test]
async fn a_stream_without_thinking_is_byte_identical() {
    let frames = [
        "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"m\"}}\r\n\r\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        TEXT_DELTA,
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ];
    let joined = frames.concat();
    // Split mid-frame so the framing, not the chunking, is what gets checked.
    let (head, tail) = joined.split_at(40);
    assert_eq!(
        collect(vec![chunk(head), chunk(tail)], Some(THIRD_PARTY)).await,
        joined
    );
}

#[tokio::test]
async fn an_oversized_frame_is_forwarded_verbatim_and_framing_resumes() {
    let big = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\n\n",
        "x".repeat(256)
    );
    let (head, tail) = big.split_at(big.len() - 1);
    let start = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"SIG\"}}\n\n";
    let stop = "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n";
    // The boundary of the oversized frame straddles two chunks.
    let out = collect_within(
        vec![chunk(head), chunk(&format!("{tail}{start}{stop}"))],
        Some(THIRD_PARTY),
        64,
    )
    .await;
    assert!(
        out.starts_with(&big),
        "the oversized frame is forwarded as it stands"
    );
    assert_eq!(
        signature_deltas(&out).len(),
        1,
        "and the frames after its boundary are framed — and wrapped — again"
    );
}

// ------------------------------------------------------------ relay_response

fn relay_state() -> crate::server::AppState {
    let mut config = Config::default();
    config.providers.insert(
        "minimax".into(),
        provider(THIRD_PARTY, AuthMode::Passthrough),
    );
    config.providers.insert(
        "claude".into(),
        provider("http://127.0.0.1:9", AuthMode::ClaudeOauth),
    );
    crate::server::AppState::new(config, reqwest::Client::new()).unwrap()
}

fn route(provider: &str) -> crate::routing::Route {
    crate::routing::Route {
        provider: provider.to_string(),
        adapter: crate::routing::AdapterKind::Anthropic,
        model: "m".to_string(),
        upstream_model: "m".to_string(),
        effort: None,
        service_tier: None,
    }
}

async fn relayed(provider: &str, content_type: &str, body: &str) -> String {
    let upstream = reqwest::Response::from(
        axum::http::Response::builder()
            .status(200)
            .header("content-type", content_type)
            .body(reqwest::Body::from(body.to_owned()))
            .unwrap(),
    );
    let (_, response) = super::super::relay_response(
        &relay_state(),
        &route(provider),
        upstream,
        None,
        crate::adapters::ResponseBounds {
            max_bytes: None,
            idle: None,
        },
    )
    .await
    .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The twin pair at the hook every relay goes through: the same body is
/// wrapped for the third-party provider and left byte-identical for
/// api.anthropic.com and a `claude_oauth` pool, streamed and buffered alike.
#[tokio::test]
async fn relay_response_wraps_third_party_signatures_only() {
    let json_body = r#"{"content":[{"type":"thinking","thinking":"t","signature":"orig"}]}"#;
    let sse_body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"orig\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n";

    let json_out = relayed("minimax", "application/json", json_body).await;
    let value: Value = serde_json::from_str(&json_out).unwrap();
    assert_eq!(
        unwrap_upstream_signature(value["content"][0]["signature"].as_str().unwrap()),
        Some((THIRD_PARTY.to_string(), "orig".to_string()))
    );
    let sse_out = relayed("minimax", "text/event-stream", sse_body).await;
    assert_eq!(
        signature_deltas(&sse_out)
            .into_iter()
            .map(|(_, signature)| unwrap_upstream_signature(&signature))
            .collect::<Vec<_>>(),
        vec![Some((THIRD_PARTY.to_string(), "orig".to_string()))]
    );

    for provider in ["anthropic", "claude"] {
        assert_eq!(
            relayed(provider, "application/json", json_body).await,
            json_body,
            "{provider}: a buffered body stays passthrough"
        );
        assert_eq!(
            relayed(provider, "text/event-stream", sse_body).await,
            sse_body,
            "{provider}: a stream stays passthrough"
        );
    }
}

// --------------------------------------------------------------- full loop

async fn last_received(server: &wiremock::MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    serde_json::from_slice(&requests.last().unwrap().body).unwrap()
}

/// The whole round trip through `forward`, against wiremock upstreams: a
/// third-party response leaves wrapped, the wrapped transcript goes back to
/// that provider carrying the original signature, and the same transcript sent
/// to a `claude_oauth` pool arrives with the thinking block removed.
#[tokio::test]
async fn a_third_party_signature_round_trips_home_and_never_reaches_the_claude_pool() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let minimax = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [
                {"type": "thinking", "thinking": "weighing it", "signature": "mm-original"},
                {"type": "text", "text": "done"},
            ],
        })))
        .mount(&minimax)
        .await;
    let claude = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"content": []})))
        .mount(&claude)
        .await;

    let credentials = std::env::temp_dir().join(format!(
        "shunt-thinking-wrap-{}-claude.json",
        std::process::id()
    ));
    std::fs::write(
        &credentials,
        r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":4000000000000}}"#,
    )
    .unwrap();
    let mut config = Config::default();
    // Passthrough: the client's own header is the credential, so nothing reads
    // the environment.
    config.providers.insert(
        "minimax".into(),
        provider(&minimax.uri(), AuthMode::Passthrough),
    );
    let mut pool = provider(&claude.uri(), AuthMode::ClaudeOauth);
    pool.accounts = vec![crate::config::AccountConfig {
        name: "acct".to_string(),
        credentials: Some(credentials.display().to_string()),
        uuid: Some("uuid-acct".to_string()),
        ..Default::default()
    }];
    config.providers.insert("claude".into(), pool);
    let state = crate::server::AppState::new(config, reqwest::Client::new()).unwrap();

    let uri: axum::http::Uri = "/v1/messages".parse().unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-api-key", "client-key".parse().unwrap());
    headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
    let bounds = || crate::adapters::ResponseBounds {
        max_bytes: None,
        idle: None,
    };
    let send = |provider: &'static str, request: Value| {
        let state = state.clone();
        let uri = uri.clone();
        let headers = headers.clone();
        async move {
            let (_, response) = super::super::forward(
                state,
                route(provider),
                &uri,
                &headers,
                body(request),
                bounds(),
            )
            .await
            .unwrap_or_else(|_| panic!("{provider} relays"));
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
        }
    };

    // 1. The third-party response reaches the client wrapped.
    let first: Value = serde_json::from_slice(
        &send(
            "minimax",
            json!({"model": "m", "messages": [{"role": "user", "content": "go"}]}),
        )
        .await,
    )
    .unwrap();
    let wrapped = first["content"][0]["signature"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        unwrap_upstream_signature(&wrapped),
        Some((minimax.uri(), "mm-original".to_string()))
    );

    let next_turn = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": first["content"]},
            {"role": "user", "content": "and then?"},
        ],
    });

    // 2. Back to the same provider: the original signature, byte for byte.
    send("minimax", next_turn.clone()).await;
    let home = last_received(&minimax).await;
    assert_eq!(
        home["messages"][1]["content"][0]["signature"], "mm-original",
        "the issuing host gets its own signature back"
    );

    // 3. To the Claude pool: the block is gone, the rest of the turn intact.
    send("claude", next_turn).await;
    let away = last_received(&claude).await;
    let _ = std::fs::remove_file(&credentials);
    assert_eq!(
        away["messages"][1]["content"],
        json!([{"type": "text", "text": "done"}]),
        "a signature Anthropic cannot verify never reaches it"
    );
}
