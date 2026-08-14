use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::auth::inbound::{is_consumed_by_shunt, InboundAuth};
use crate::config::{AuthMode, Config};
use crate::gateway::{approval::Identity, jwt, GatewayAuth};
use crate::routing::{AdapterKind, Route};
use crate::server::AppState;

use super::{check_inbound_auth, headers_for_route, InboundContext};

const GATEWAY_URL: &str = "https://gateway.example";
const GATEWAY_SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const STATIC_TOKEN: &str = "static-secret-token";

fn passthrough_route() -> Route {
    Route {
        provider: "anthropic".to_string(),
        adapter: AdapterKind::Anthropic,
        model: "claude-opus-5".to_string(),
        upstream_model: "claude-opus-5".to_string(),
        effort: None,
        service_tier: None,
    }
}

/// A credential-injecting fallback route (`openai`, `AuthMode::ApiKey` by
/// default) — used only by the mixed-chain test below to make the whole
/// chain `injects_credential`, so `check_inbound_auth` runs the real
/// client-token gate instead of the `!injects_credential` shortcut every
/// other fixture in this file relies on.
fn injecting_fallback_route() -> Route {
    Route {
        provider: "openai".to_string(),
        adapter: AdapterKind::Responses,
        model: "gpt-5".to_string(),
        upstream_model: "gpt-5".to_string(),
        effort: None,
        service_tier: None,
    }
}

/// A gateway-enabled state with a single passthrough Anthropic provider.
/// `gateway_auth` is always configured so the gateway-JWT check has a real
/// key to verify against; a fixture header that is not shunt's JWT never
/// matches it regardless.
fn state() -> AppState {
    let mut config = Config::default();
    config.providers.get_mut("anthropic").unwrap().auth = AuthMode::Passthrough;
    let mut state = AppState::new(config, reqwest::Client::new()).unwrap();
    state.gateway_auth = Some(Arc::new(GatewayAuth::with_optional_approval(
        GATEWAY_URL.to_string(),
        GATEWAY_SECRET.to_vec(),
        3600,
        false,
        None,
    )));
    state
}

/// A configured `[server.auth]` static client token, independent of the
/// gateway JWT fixture above.
fn static_inbound_auth() -> InboundAuth {
    InboundAuth::new(
        HeaderName::from_static("x-shunt-token"),
        vec![("client".to_string(), STATIC_TOKEN.to_string())],
    )
}

/// Like [`state`], but also configures a static `[server.auth]` token, so
/// tests can exercise the by-value static-token check independently of the
/// gateway-JWT one (both are configured at once, matching how a real
/// deployment mixes gateway login with static client tokens).
fn state_with_static_auth() -> AppState {
    let mut state = state();
    state.inbound_auth = Some(Arc::new(static_inbound_auth()));
    state
}

/// A real gateway JWT, minted against the same secret/issuer `state()` verifies
/// with — not a fixture string shaped like one.
fn gateway_jwt() -> String {
    jwt::mint(
        &Identity {
            sub: "dev".to_string(),
            email: "dev@example.com".to_string(),
            name: "Dev".to_string(),
        },
        GATEWAY_URL,
        GATEWAY_SECRET,
        3600,
    )
}

/// Both slots hold the caller's own, non-gateway credential — the shape an
/// `apiKeyHelper` produces for a plain upstream token.
fn caller_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer caller"));
    headers.insert("x-api-key", HeaderValue::from_static("caller"));
    headers
}

/// Build the `InboundContext` the way `check_inbound_auth` would for these
/// headers, on the route chain every test in this file uses: a chain of
/// nothing but passthrough entries. That takes `check_inbound_auth`'s
/// `!injects_credential` early return, which never runs the client-token gate
/// at all, so the real function always produces `client: None` and
/// `static_client: false` here — not a value derived from whether a gateway
/// JWT happens to be present. `gateway_claims` reflects whatever
/// `authorization` actually verifies as, rather than a claim asserted
/// independently of the headers.
fn context_for(state: &AppState, headers: &HeaderMap) -> InboundContext {
    let gateway_claims = state
        .gateway_auth
        .as_ref()
        .and_then(|auth| auth.authenticate_bearer(headers));
    InboundContext {
        client: None,
        static_client: false,
        gateway_claims,
    }
}

/// The caller's own upstream Anthropic key. It is never shunt's, so no strip
/// rule in this file may touch it.
const GENUINE_UPSTREAM_KEY: &str = "sk-ant-genuine-upstream-key";

/// The mixed-slot request shape the per-slot rule turns on: a shunt-owned
/// credential in `Authorization`, the caller's genuine upstream key in
/// `x-api-key`. A request-level (rather than per-slot) strip decision cannot
/// tell these two apart, which is what the assertion below detects.
fn mixed_slot_headers(bearer: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    headers.insert("x-api-key", GENUINE_UPSTREAM_KEY.parse().unwrap());
    headers
}

/// Assert what `mixed_slot_headers` is built to detect: only the slot holding
/// the shunt-owned credential was stripped.
fn assert_only_bearer_slot_stripped(result: &HeaderMap) {
    assert!(result.get("authorization").is_none());
    assert_eq!(result.get("x-api-key").unwrap(), GENUINE_UPSTREAM_KEY);
}

#[test]
fn same_origin_passthrough_strips_only_the_slot_holding_the_gateway_jwt() {
    // The regression (#352 follow-up): a caller logged into the gateway
    // (JWT in `Authorization`) who also carries a genuine upstream Anthropic
    // key in `x-api-key` must keep that key — only the JWT-bearing slot goes.
    let state = state();
    let headers = mixed_slot_headers(&gateway_jwt());
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_only_bearer_slot_stripped(&result);
}

#[test]
fn same_origin_passthrough_strips_both_slots_when_both_hold_the_gateway_jwt() {
    // The `apiKeyHelper` shape: it fills both `Authorization` and `x-api-key`
    // with the same value, so a gateway JWT delivered that way must be
    // stripped from both.
    let state = state();
    let jwt = gateway_jwt();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {jwt}").parse().unwrap());
    headers.insert("x-api-key", jwt.parse().unwrap());
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert!(result.get("authorization").is_none());
    assert!(result.get("x-api-key").is_none());
}

#[test]
fn same_origin_passthrough_strips_a_gateway_jwt_found_only_in_the_api_key_slot() {
    // The residual hole this fix also closes: verification used to read only
    // `authorization`, so a gateway JWT arriving solely in `x-api-key` was
    // relayed upstream.
    let state = state();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", gateway_jwt().parse().unwrap());
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert!(result.get("x-api-key").is_none());
}

#[test]
fn same_origin_passthrough_keeps_both_slots_without_a_gateway_jwt() {
    // Non-vacuity control: an arbitrary caller credential in both slots, with
    // no gateway JWT anywhere, must survive unchanged. This varies only the
    // gateway-JWT dimension relative to the two tests above, so it isolates
    // what they are actually asserting.
    let state = state();
    let headers = caller_headers();
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_eq!(result.get("authorization").unwrap(), "Bearer caller");
    assert_eq!(result.get("x-api-key").unwrap(), "caller");
}

#[test]
fn off_origin_failover_strips_both_slots_without_a_gateway_jwt() {
    // A passthrough failover attempt to a different origin than the primary's
    // still strips both slots outright, independent of the gateway-JWT check.
    let state = state();
    let headers = caller_headers();
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(
        &state,
        &passthrough_route(),
        &headers,
        &inbound,
        false,
        Some("https://other.example"),
    );

    assert!(result.get("authorization").is_none());
    assert!(result.get("x-api-key").is_none());
}

#[test]
fn ungated_passthrough_chain_still_strips_only_the_gateway_jwt_slot() {
    // `check_inbound_auth`'s `!injects_credential` early return (a pure
    // passthrough chain, never gated) still carries `gateway_claims` into
    // `headers_for_route`, so the per-slot rule applies identically here: the
    // JWT-bearing slot goes, the genuine upstream key survives.
    let state = state();
    let headers = mixed_slot_headers(&gateway_jwt());
    let routes = vec![passthrough_route()];

    let (base_headers, inbound) = match check_inbound_auth(&state, &routes, &headers) {
        Ok(result) => result,
        Err(error) => panic!("check_inbound_auth rejected the request: {}", error.message),
    };
    let result = headers_for_route(
        &state,
        &passthrough_route(),
        &base_headers,
        &inbound,
        true,
        None,
    );

    assert_only_bearer_slot_stripped(&result);
}

#[test]
fn non_utf8_x_api_key_is_not_shunts_gateway_jwt_and_is_forwarded() {
    // `consumed_by`'s gateway-JWT check has an explicit non-UTF-8 fallback
    // (`str::from_utf8(value).is_ok_and(...)`) with no dedicated coverage: a
    // header value that fails UTF-8 decoding can't possibly be a JWT (a
    // base64url/HS256 string), so it must be treated as not shunt's and kept,
    // not silently stripped alongside a real one.
    let state = state();
    let non_utf8 = HeaderValue::from_bytes(&[0xff, 0xfe, b'x']).expect("opaque header value");
    assert!(!is_consumed_by_shunt(
        non_utf8.as_bytes(),
        state.gateway_auth.as_deref(),
        state.inbound_auth.as_deref()
    ));

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", non_utf8.clone());
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_eq!(
        result.get("x-api-key").unwrap().as_bytes(),
        non_utf8.as_bytes()
    );
}

#[test]
fn same_origin_passthrough_strips_a_static_token_in_the_api_key_slot() {
    // #357: a configured `[server.auth]` token forwarded in `x-api-key` must
    // never reach the upstream, exactly like a gateway JWT already didn't.
    let state = state_with_static_auth();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static(STATIC_TOKEN));
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert!(result.get("x-api-key").is_none());
}

#[test]
fn same_origin_passthrough_strips_a_static_token_in_the_authorization_slot() {
    // #357: the same static token, delivered as `Authorization: Bearer`
    // instead of `x-api-key`, must also be stripped.
    let state = state_with_static_auth();
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {STATIC_TOKEN}").parse().unwrap(),
    );
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert!(result.get("authorization").is_none());
}

#[test]
fn same_origin_passthrough_keeps_a_genuine_credential_even_with_static_auth_configured() {
    // Non-vacuity control for the two tests above: merely configuring
    // `[server.auth]` must not itself cause stripping — only a slot that
    // actually holds the configured token does. This varies only the
    // static-token-ness dimension relative to those two tests, so it isolates
    // what they are actually asserting.
    let state = state_with_static_auth();
    let headers = caller_headers();
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_eq!(result.get("authorization").unwrap(), "Bearer caller");
    assert_eq!(result.get("x-api-key").unwrap(), "caller");
}

#[test]
fn same_origin_passthrough_strips_only_the_slot_holding_the_static_token() {
    // The static-token counterpart of the gateway-JWT mixed-slot test above:
    // the static token in `Authorization` is stripped, while a genuine
    // upstream key sitting in `x-api-key` at the same time survives.
    let state = state_with_static_auth();
    let headers = mixed_slot_headers(STATIC_TOKEN);
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_only_bearer_slot_stripped(&result);
}

#[test]
fn gated_mixed_chain_never_forwards_the_static_token_used_to_pass_the_gate() {
    // The exact leak path #357 describes: a chain whose primary
    // (`anthropic`) is passthrough but whose fallback (`openai`) injects a
    // credential is chain-level `injects_credential`, so `check_inbound_auth`
    // runs the real client-token gate — not the `!injects_credential`
    // shortcut every other fixture in this file takes via `context_for`. The
    // static token that authenticated against that gate must still never
    // reach the primary passthrough attempt's upstream headers.
    let state = state_with_static_auth();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static(STATIC_TOKEN));
    let routes = vec![passthrough_route(), injecting_fallback_route()];

    let (base_headers, inbound) = match check_inbound_auth(&state, &routes, &headers) {
        Ok(result) => result,
        Err(error) => panic!("check_inbound_auth rejected the request: {}", error.message),
    };
    let result = headers_for_route(
        &state,
        &passthrough_route(),
        &base_headers,
        &inbound,
        true,
        None,
    );

    assert!(result.get("x-api-key").is_none());
}
