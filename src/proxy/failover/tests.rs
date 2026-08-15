use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::admin::AdminAuth;
use crate::auth::inbound::{is_consumed_by_shunt, InboundAuth};
use crate::config::{AdminKey, AdminKeyring, AuthMode, Config, Secret};
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
        state.inbound_auth.as_deref(),
        None
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
fn same_origin_passthrough_strips_an_unprefixed_static_token_in_the_authorization_slot() {
    // `[server.auth] header = "authorization"` lets a caller pass the gate with
    // a bare `Authorization: <token>` and no `Bearer ` scheme, which a
    // Bearer-payload-only check cannot see — the shape that was leaking on the
    // discovery path. `check_inbound_auth` removes the configured header before
    // this function runs, so on the inference path this is defence in depth
    // rather than a live leak; it is asserted here because both passthrough
    // paths are documented to agree on what counts as shunt's own credential,
    // and only a test keeps that agreement true.
    let state = state_with_static_auth();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static(STATIC_TOKEN));
    headers.insert("x-api-key", HeaderValue::from_static(GENUINE_UPSTREAM_KEY));
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

/// A different secret than the one `state()`/`gateway_auth()` verify with —
/// for minting a well-formed, `aud = "shunt"` token that fails signature
/// verification (e.g. after a secret rotation) but is still shape-recognized.
const OTHER_SECRET: &[u8] = b"fedcba9876543210fedcba9876543210";

/// A different `public_url` than `GATEWAY_URL` — for minting a token as if by
/// a sibling instance sharing the same `jwt_secret` but configured under a
/// different issuer.
const SIBLING_URL: &str = "https://sibling.gateway.example";

fn identity_for_test() -> Identity {
    Identity {
        sub: "dev".to_string(),
        email: "dev@example.com".to_string(),
        name: "Dev".to_string(),
    }
}

#[test]
fn expired_gateway_jwt_in_x_api_key_is_not_forwarded() {
    // #358 case 1: shunt itself minted this token (aud = "shunt", real
    // secret), but its ttl is 0, so it is already expired by the time
    // `verify_at` runs. `authenticate_token` therefore returns `None`, but it
    // is still shunt's own credential and must not reach the upstream.
    let state = state();
    let expired = jwt::mint(&identity_for_test(), GATEWAY_URL, GATEWAY_SECRET, 0);
    assert!(state
        .gateway_auth
        .as_ref()
        .unwrap()
        .authenticate_token(&expired)
        .is_none());

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", expired.parse().unwrap());
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert!(result.get("x-api-key").is_none());
}

#[test]
fn wrong_issuer_gateway_jwt_in_authorization_is_not_forwarded() {
    // #358 case 2, the sharper one: a fleet sharing one `jwt_secret` across
    // differing `public_url` values. This token is minted for a sibling
    // instance, is still live, and fails `authenticate_token` under
    // `GATEWAY_URL` purely on issuer mismatch — but its `aud` is still
    // "shunt", so it must still be recognized and stripped. The genuine
    // upstream key in `x-api-key` must survive untouched.
    let state = state();
    let sibling_token = jwt::mint(&identity_for_test(), SIBLING_URL, GATEWAY_SECRET, 3600);
    assert!(state
        .gateway_auth
        .as_ref()
        .unwrap()
        .authenticate_token(&sibling_token)
        .is_none());

    let headers = mixed_slot_headers(&sibling_token);
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_only_bearer_slot_stripped(&result);
}

#[test]
fn bad_signature_gateway_jwt_is_not_forwarded() {
    // #358 case 3: minted under a different secret (e.g. post-rotation), so
    // `authenticate_token` rejects it on signature — but it is well-formed
    // with `aud = "shunt"`, so shape still catches it.
    let state = state();
    let token = jwt::mint(&identity_for_test(), GATEWAY_URL, OTHER_SECRET, 3600);
    assert!(state
        .gateway_auth
        .as_ref()
        .unwrap()
        .authenticate_token(&token)
        .is_none());

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", token.parse().unwrap());
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert!(result.get("x-api-key").is_none());
}

#[test]
fn bare_authorization_header_with_an_invalid_shunt_jwt_is_not_forwarded() {
    // #358 case 4: `[server.auth] header = "authorization"` lets a bare
    // `Authorization: <token>` (no `Bearer ` scheme) pass the gate, so the
    // by-value check must also see the raw header value, not just the Bearer
    // payload. Here the raw value is an expired shunt-minted JWT.
    let state = state();
    let expired = jwt::mint(&identity_for_test(), GATEWAY_URL, GATEWAY_SECRET, 0);
    let mut headers = HeaderMap::new();
    headers.insert("authorization", expired.parse().unwrap());
    headers.insert("x-api-key", HeaderValue::from_static(GENUINE_UPSTREAM_KEY));
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_only_bearer_slot_stripped(&result);
}

#[test]
fn garbage_three_segment_string_is_not_treated_as_shunts_and_is_forwarded() {
    // Malformed-input control: a string with the right segment count but no
    // valid base64/JSON payload must not panic and must not be treated as
    // shunt's own credential.
    let state = state();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("a.b.c"));
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_eq!(result.get("x-api-key").unwrap(), "a.b.c");
}

// --- Admin credentials (#346) ------------------------------------------------
//
// Making `x-api-key` an accepted admin slot (`AdminAuth::authenticate_credential`)
// widens what shunt itself consumes, so the strip predicate has to widen with it:
// whatever slot and shape can authenticate an admin credential must be stripped
// in that same slot and shape before anything goes upstream. Admin credentials
// provision upstream accounts, so forwarding one verbatim to a provider is a
// strictly worse leak than the static-token case above.

const ADMIN_TOKEN: &str = "admin-legacy-token-0123456789abcd";
const ADMIN_WRITE_KEY: &str = "admin-write-key-0123456789abcdef0";
const ADMIN_READ_KEY: &str = "admin-read-key-0123456789abcdef01";

/// A resolved admin keyring holding one credential of each of the three kinds
/// `AdminKeyring` can authenticate, under `header`. The header name matters:
/// `[server.admin] header` is free-form and may even be `authorization`, which
/// is what the `authorization`-slot tests below configure.
fn admin_auth_with_header(header: &'static str) -> AdminAuth {
    AdminAuth::new(
        HeaderName::from_static(header),
        AdminKeyring::new(
            &[("ops".to_string(), ADMIN_TOKEN.to_string())],
            &[AdminKey {
                id: "writer".to_string(),
                key: Secret::from(ADMIN_WRITE_KEY),
            }],
            &[AdminKey {
                id: "reader".to_string(),
                key: Secret::from(ADMIN_READ_KEY),
            }],
        ),
        Duration::from_secs(3600),
        Duration::from_secs(600),
    )
}

/// Like [`state`], but with `[server.admin]` resolved under `header`.
fn state_with_admin_auth(header: &'static str) -> AppState {
    let mut state = state();
    state.admin_auth = Some(Arc::new(admin_auth_with_header(header)));
    state
}

/// The mirror of [`mixed_slot_headers`]: the credential under test sits in
/// `x-api-key` while the caller's genuine upstream key sits in `Authorization`,
/// so a request-level strip decision would take the genuine one down with it.
fn admin_in_api_key_slot(credential: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {GENUINE_UPSTREAM_KEY}").parse().unwrap(),
    );
    headers.insert("x-api-key", credential.parse().unwrap());
    headers
}

/// Assert what [`admin_in_api_key_slot`] is built to detect: only the slot
/// holding the admin credential was stripped.
fn assert_only_api_key_slot_stripped(result: &HeaderMap) {
    assert!(result.get("x-api-key").is_none());
    assert_eq!(
        result.get("authorization").unwrap(),
        format!("Bearer {GENUINE_UPSTREAM_KEY}").as_str()
    );
}

#[test]
fn same_origin_passthrough_strips_an_admin_token_in_the_api_key_slot() {
    let state = state_with_admin_auth("x-shunt-admin");
    let headers = admin_in_api_key_slot(ADMIN_TOKEN);
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_only_api_key_slot_stripped(&result);
}

#[test]
fn same_origin_passthrough_strips_an_admin_write_key_in_the_api_key_slot() {
    let state = state_with_admin_auth("x-shunt-admin");
    let headers = admin_in_api_key_slot(ADMIN_WRITE_KEY);
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_only_api_key_slot_stripped(&result);
}

#[test]
fn same_origin_passthrough_strips_an_admin_read_key_in_the_api_key_slot() {
    // A read key authenticates the admin GETs, so it is just as much a shunt
    // credential as the write tiers — enumerating only the write sets would
    // leave this one forwarded upstream.
    let state = state_with_admin_auth("x-shunt-admin");
    let headers = admin_in_api_key_slot(ADMIN_READ_KEY);
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_only_api_key_slot_stripped(&result);
}

#[test]
fn same_origin_passthrough_strips_an_admin_credential_in_the_authorization_slot() {
    // `[server.admin] header = "authorization"` is legal — `InvalidAdminHeader`
    // only checks the name parses — so the admin credential arrives in the
    // `authorization` slot in both shapes `AdminAuth` accepts: the bare value
    // (what the configured header carries) and `Bearer <value>`.
    for shape in [
        ADMIN_WRITE_KEY.to_string(),
        format!("Bearer {ADMIN_WRITE_KEY}"),
    ] {
        let state = state_with_admin_auth("authorization");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", shape.parse().unwrap());
        headers.insert("x-api-key", GENUINE_UPSTREAM_KEY.parse().unwrap());
        let inbound = context_for(&state, &headers);

        let result =
            headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

        assert_only_bearer_slot_stripped(&result);
    }
}

#[test]
fn same_origin_passthrough_keeps_a_genuine_credential_with_no_admin_configured() {
    // Non-vacuity control #1 for the four tests above: without `[server.admin]`
    // resolved at all, the same slots carry the caller's own credential through
    // untouched, so those tests cannot be passing because everything is
    // stripped.
    let state = state();
    let headers = admin_in_api_key_slot(GENUINE_UPSTREAM_KEY);
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_eq!(result.get("x-api-key").unwrap(), GENUINE_UPSTREAM_KEY);
    assert_eq!(
        result.get("authorization").unwrap(),
        format!("Bearer {GENUINE_UPSTREAM_KEY}").as_str()
    );
}

#[test]
fn same_origin_passthrough_keeps_an_unrelated_value_with_admin_configured() {
    // Non-vacuity control #2: configuring `[server.admin]` must not itself
    // cause stripping — only a slot whose *value* is one of the three admin
    // credential sets does. This varies only the admin-credential-ness
    // dimension relative to the tests above.
    let state = state_with_admin_auth("x-shunt-admin");
    let headers = admin_in_api_key_slot(GENUINE_UPSTREAM_KEY);
    let inbound = context_for(&state, &headers);

    let result = headers_for_route(&state, &passthrough_route(), &headers, &inbound, true, None);

    assert_eq!(result.get("x-api-key").unwrap(), GENUINE_UPSTREAM_KEY);
    assert_eq!(
        result.get("authorization").unwrap(),
        format!("Bearer {GENUINE_UPSTREAM_KEY}").as_str()
    );
}

#[test]
fn check_inbound_auth_removes_the_configured_admin_header() {
    // The dedicated `[server.admin]` header is a slot shunt consumes, so it is
    // dropped outright before forwarding, exactly like the `[server.auth]` one.
    // The caller's own credentials in the other slots are untouched, which is
    // what distinguishes "removed the admin header" from "removed everything".
    let state = state_with_admin_auth("x-shunt-admin");
    let mut headers = caller_headers();
    headers.insert("x-shunt-admin", ADMIN_WRITE_KEY.parse().unwrap());

    let (forwarded, _) = check_inbound_auth(&state, &[passthrough_route()], &headers)
        .unwrap_or_else(|error| {
            panic!("check_inbound_auth rejected the request: {}", error.message)
        });

    assert!(forwarded.get("x-shunt-admin").is_none());
    assert_eq!(forwarded.get("authorization").unwrap(), "Bearer caller");
    assert_eq!(forwarded.get("x-api-key").unwrap(), "caller");
}
