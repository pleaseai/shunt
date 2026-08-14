use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue};

use crate::config::{AuthMode, Config};
use crate::gateway::{approval::Identity, jwt, GatewayAuth};
use crate::routing::{AdapterKind, Route};
use crate::server::AppState;

use super::{check_inbound_auth, headers_for_route, InboundContext};

const GATEWAY_URL: &str = "https://gateway.example";
const GATEWAY_SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

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

/// A gateway-enabled state with a single passthrough Anthropic provider.
/// `gateway_auth` is always configured so `is_gateway_jwt` has a real key to
/// verify against; a fixture header that is not shunt's JWT never matches it
/// regardless.
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
/// headers: `gateway_claims` reflects whatever `authorization` actually
/// verifies as, rather than a claim asserted independently of the headers.
fn context_for(state: &AppState, headers: &HeaderMap) -> InboundContext {
    let gateway_claims = state
        .gateway_auth
        .as_ref()
        .and_then(|auth| auth.authenticate_bearer(headers));
    InboundContext {
        client: Some("dev@example.com".to_string()),
        static_client: gateway_claims.is_none(),
        gateway_claims,
    }
}

#[test]
fn same_origin_passthrough_strips_only_the_slot_holding_the_gateway_jwt() {
    // The regression (#352 follow-up): a caller logged into the gateway
    // (JWT in `Authorization`) who also carries a genuine upstream Anthropic
    // key in `x-api-key` must keep that key — only the JWT-bearing slot goes.
    let state = state();
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", gateway_jwt()).parse().unwrap(),
    );
    headers.insert(
        "x-api-key",
        HeaderValue::from_static("sk-ant-genuine-upstream-key"),
    );
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert!(result.get("authorization").is_none());
    assert_eq!(
        result.get("x-api-key").unwrap(),
        "sk-ant-genuine-upstream-key"
    );
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
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", gateway_jwt()).parse().unwrap(),
    );
    headers.insert(
        "x-api-key",
        HeaderValue::from_static("sk-ant-genuine-upstream-key"),
    );
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

    assert!(result.get("authorization").is_none());
    assert_eq!(
        result.get("x-api-key").unwrap(),
        "sk-ant-genuine-upstream-key"
    );
}
