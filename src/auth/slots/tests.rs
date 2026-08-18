//! The mirror test: the invariant from the module docs, encoded directly.
//!
//! Rather than testing one scenario at a time, this walks the cross product of
//! every credential kind shunt owns and every delivery shape a slot can carry
//! it in, computes acceptance by calling the **real** accept predicates, and
//! then asserts that whatever was accepted is gone from every registered
//! forward site's output. A hand-written expectation table would stop being a
//! mirror the moment an accept predicate widened, which is exactly the failure
//! mode this file exists to catch.

use std::{collections::BTreeSet, sync::Arc};

use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::{
    adapters::responses::inbound::passthrough_request_headers,
    admin::AdminAuth,
    auth::inbound::InboundAuth,
    config::{AdminKey, AdminKeyring, AuthMode, Config, Secret},
    discovery::upstream::upstream_headers,
    gateway::{approval::Identity, jwt, GatewayAuth},
    proxy::failover::{check_inbound_auth, headers_for_route},
    routing::{AdapterKind, Route},
    server::AppState,
};

use super::ShuntCredentials;

const GATEWAY_URL: &str = "https://gateway.example";
const GATEWAY_SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
/// A sibling instance's issuer: a JWT minted for it still has shunt's shape but
/// does not verify here.
const SIBLING_URL: &str = "https://sibling.example";

const STATIC_TOKEN: &str = "static-secret-token";
const ADMIN_WRITE_KEY: &str = "admin-write-key-0123456789abcdef0";
const ADMIN_READ_KEY: &str = "admin-read-key-0123456789abcdef01";
const ADMIN_LEGACY_TOKEN: &str = "admin-legacy-token-0123456789abcd";

/// The caller's own upstream credential. No accept predicate matches it, so no
/// strip rule anywhere in this file may touch it on a shared-slot forward.
const GENUINE_UPSTREAM_KEY: &str = "sk-ant-genuine-upstream-key";

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

/// Every credential surface configured at once, the way a real shared gateway
/// mixes them: `[server.gateway]` login, a static `[server.auth]` token, and a
/// `[server.admin]` keyring holding one credential of each of the three kinds.
/// Header names are the defaults, so the fixture matches what an operator gets
/// without extra configuration.
fn state() -> AppState {
    state_with_headers(
        HeaderName::from_static("x-shunt-token"),
        HeaderName::from_static("x-shunt-admin-token"),
    )
}

fn state_with_admin_header(admin_header: HeaderName) -> AppState {
    state_with_headers(HeaderName::from_static("x-shunt-token"), admin_header)
}

fn state_with_headers(static_header: HeaderName, admin_header: HeaderName) -> AppState {
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
    state.inbound_auth = Some(Arc::new(InboundAuth::new(
        static_header,
        vec![("client".to_string(), STATIC_TOKEN.to_string())],
    )));
    state.admin_auth = Some(Arc::new(AdminAuth::new(
        admin_header,
        admin_keyring(),
        std::time::Duration::from_secs(3600),
        std::time::Duration::from_secs(600),
    )));
    state
}

/// One configuration the whole mirror table is evaluated under.
///
/// The defaults alone are not enough. `[server.auth] header` and
/// `[server.admin] header` are free-form names, and pointing one at a slot the
/// caller also uses is what makes the *raw* `authorization` shape an accepted
/// credential at all — that is the #361 defect, where the strip predicate
/// covered only the `Bearer` payload. Under the default names no accept
/// predicate ever reads `authorization` raw, so the invariant would not bind on
/// that shape and the row would be dead weight.
struct Fixture {
    name: &'static str,
    state: AppState,
    /// How many `(kind, shape)` pairs the real accept predicates admit under
    /// this configuration. Hard-coded so a refactor that makes every accept
    /// predicate return `None` fails loudly here instead of quietly turning the
    /// mirror assertion into a tautology.
    accepted_pairs: usize,
}

fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "defaults",
            // The verifying gateway JWT in `Bearer` (1); the static token in
            // `Bearer`, `x-api-key`, and its own header (3); each of the three
            // admin credentials in `x-api-key` and the admin header (6).
            accepted_pairs: 10,
            state: state(),
        },
        Fixture {
            // Also exercises the deliberate asymmetry in `strip_reserved_slots`:
            // the static header is dropped by name even though it now names a
            // shared slot, while the admin header is not and is instead cleared
            // by value.
            name: "[server.auth] header = authorization, [server.admin] header = x-api-key",
            // The verifying gateway JWT in `Bearer` (1); the static token in all
            // five shapes, since two of them now *are* its configured header
            // (5); each of the three admin credentials in the two shapes that
            // name `x-api-key` (6).
            accepted_pairs: 12,
            state: state_with_headers(
                HeaderName::from_static("authorization"),
                HeaderName::from_static("x-api-key"),
            ),
        },
    ]
}

fn admin_keyring() -> AdminKeyring {
    AdminKeyring::new(
        &[("ops".to_string(), ADMIN_LEGACY_TOKEN.to_string())],
        &[AdminKey {
            id: "writer".to_string(),
            key: Secret::from(ADMIN_WRITE_KEY),
        }],
        &[AdminKey {
            id: "reader".to_string(),
            key: Secret::from(ADMIN_READ_KEY),
        }],
    )
}

/// A real gateway JWT, minted against the same secret and issuer `state()`
/// verifies with — not a fixture string shaped like one.
fn gateway_jwt(issuer: &str) -> String {
    jwt::mint(
        &Identity {
            sub: "dev".to_string(),
            email: "dev@example.com".to_string(),
            name: "Dev".to_string(),
        },
        issuer,
        GATEWAY_SECRET,
        3600,
    )
}

// --- The mirror table --------------------------------------------------------

/// One of shunt's own credentials. Every kind here must be unforwardable; the
/// invariant only *requires* it for kinds some accept predicate matches, but
/// the sibling JWT is included precisely because it authenticates nothing and
/// must still never be relayed (forwarding it leaks the caller's identity and
/// an offline HMAC oracle for `jwt_secret`).
struct CredentialKind {
    name: &'static str,
    value: String,
}

fn credential_kinds() -> Vec<CredentialKind> {
    vec![
        CredentialKind {
            name: "gateway JWT (verifies)",
            value: gateway_jwt(GATEWAY_URL),
        },
        CredentialKind {
            name: "gateway JWT (sibling issuer; shunt-shaped, does not verify)",
            value: gateway_jwt(SIBLING_URL),
        },
        CredentialKind {
            name: "[server.auth] static token",
            value: STATIC_TOKEN.to_string(),
        },
        CredentialKind {
            name: "[server.admin] write_keys entry",
            value: ADMIN_WRITE_KEY.to_string(),
        },
        CredentialKind {
            name: "[server.admin] read_keys entry",
            value: ADMIN_READ_KEY.to_string(),
        },
        CredentialKind {
            name: "[server.admin] legacy tokens_env pair",
            value: ADMIN_LEGACY_TOKEN.to_string(),
        },
    ]
}

/// A slot plus the encoding the value arrives in. `authorization` appears
/// twice on purpose: the `Bearer` payload and the whole raw value are two
/// different shapes of the same slot, and #361 was a strip predicate that
/// covered only the first.
struct DeliveryShape {
    name: &'static str,
    header: HeaderName,
    /// How the credential value is rendered into the header value.
    render: fn(&str) -> String,
}

fn plain(value: &str) -> String {
    value.to_string()
}

fn bearer(value: &str) -> String {
    format!("Bearer {value}")
}

fn delivery_shapes(state: &AppState) -> Vec<DeliveryShape> {
    vec![
        DeliveryShape {
            name: "authorization: Bearer <v>",
            header: HeaderName::from_static("authorization"),
            render: bearer,
        },
        DeliveryShape {
            name: "authorization: <v>",
            header: HeaderName::from_static("authorization"),
            render: plain,
        },
        DeliveryShape {
            name: "x-api-key: <v>",
            header: HeaderName::from_static("x-api-key"),
            render: plain,
        },
        DeliveryShape {
            name: "<[server.auth] header>: <v>",
            header: state.inbound_auth.as_ref().unwrap().header().clone(),
            render: plain,
        },
        DeliveryShape {
            name: "<[server.admin] header>: <v>",
            header: state.admin_auth.as_ref().unwrap().header().clone(),
            render: plain,
        },
    ]
}

fn request_headers(shape: &DeliveryShape, value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        shape.header.clone(),
        HeaderValue::from_str(&(shape.render)(value)).unwrap(),
    );
    headers
}

/// Acceptance computed by the **real** predicates, never by an expectation
/// table. Every header-driven accept site in the enumeration is represented:
/// `InboundAuth`'s three entry points, `GatewayAuth`'s bearer check, and the
/// admin surface's merged header + `x-api-key` check.
fn accepted_by_any_gate(state: &AppState, headers: &HeaderMap) -> bool {
    let static_auth = state.inbound_auth.as_deref();
    let accepted_static = static_auth.is_some_and(|auth| {
        auth.authenticate(headers).is_some()
            || auth.authenticate_bearer(headers).is_some()
            || auth.authenticate_client(headers).is_some()
    });
    let accepted_gateway = state
        .gateway_auth
        .as_deref()
        .is_some_and(|auth| auth.authenticate_bearer(headers).is_some());
    let accepted_admin = state
        .admin_auth
        .as_deref()
        .is_some_and(|auth| auth.authenticate_credential(headers).is_some());
    accepted_static || accepted_gateway || accepted_admin
}

// --- The forward-site registry ----------------------------------------------

/// The three places a caller-supplied header can leave shunt for a third-party
/// upstream. Adding a fourth means adding it here, or the tripwire below fails.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ForwardSite {
    /// `proxy::failover` — `check_inbound_auth` then `headers_for_route` on the
    /// same-origin passthrough branch.
    InferenceFailover,
    /// `discovery::upstream::upstream_headers`, `AuthMode::Passthrough`.
    DiscoveryPassthrough,
    /// `adapters::responses::inbound::passthrough_request_headers`.
    CodexPassthrough,
}

const FORWARD_SITES: [ForwardSite; 3] = [
    ForwardSite::InferenceFailover,
    ForwardSite::DiscoveryPassthrough,
    ForwardSite::CodexPassthrough,
];

impl ForwardSite {
    fn name(self) -> &'static str {
        match self {
            Self::InferenceFailover => "proxy::failover (inference passthrough)",
            Self::DiscoveryPassthrough => "discovery::upstream (models passthrough)",
            Self::CodexPassthrough => "adapters::responses::inbound (codex passthrough)",
        }
    }

    /// Run the site for real and return exactly what it would hand the HTTP
    /// client. `discovery::upstream` answers `None` when nothing forwardable
    /// survives; an empty map is the faithful rendering of that.
    ///
    /// Note for anyone reading a failure here: under the `defaults` fixture the
    /// two configured-header shapes are **structurally vacuous** at
    /// [`Self::DiscoveryPassthrough`]. That site builds its outbound map from
    /// scratch and only ever considers the two shared slots, so a credential
    /// delivered in `x-shunt-token` or `x-shunt-admin-token` is not stripped
    /// there — it is simply never a candidate. That is by design, not a gap:
    /// the collision fixture points both configured headers *at* shared slots,
    /// which covers the same logic non-vacuously.
    async fn run(self, state: &AppState, headers: &HeaderMap) -> HeaderMap {
        match self {
            Self::InferenceFailover => {
                let routes = [passthrough_route()];
                // A chain of nothing but passthrough entries takes
                // `check_inbound_auth`'s `!injects_credential` early return, so
                // this never fails and never depends on the gate's verdict.
                let (forwarded, inbound) = check_inbound_auth(state, &routes, headers)
                    .unwrap_or_else(|_| unreachable!("a passthrough chain is never gated"));
                headers_for_route(state, &routes[0], &forwarded, &inbound, true, None)
            }
            Self::DiscoveryPassthrough => {
                let provider = state.config.provider("anthropic").unwrap();
                // The `Passthrough` branch resolves no credential and performs
                // no I/O, so this is a pure header computation despite `async`.
                upstream_headers(state, "anthropic", provider, headers, credentials(state))
                    .await
                    .unwrap_or_default()
            }
            Self::CodexPassthrough => passthrough_request_headers(headers, credentials(state)),
        }
    }
}

fn credentials(state: &AppState) -> ShuntCredentials<'_> {
    ShuntCredentials::from_state(state)
}

/// Scan **every** header value, not just the slot the credential arrived in: a
/// forward site that copied the value into a different header would still be
/// leaking it.
///
/// Compared over raw bytes. A lossy UTF-8 decode would replace any invalid
/// sequence with U+FFFD *before* the search, so a value adjacent to
/// non-UTF-8 bytes could be rewritten out of its own match — a false negative,
/// in the direction that hides a leak.
fn contains_value(headers: &HeaderMap, value: &str) -> bool {
    let needle = value.as_bytes();
    headers
        .iter()
        .any(|(_, header)| contains_bytes(header.as_bytes(), needle))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || (haystack.len() >= needle.len()
            && haystack
                .windows(needle.len())
                .any(|window| window == needle))
}

// --- Tests -------------------------------------------------------------------

#[tokio::test]
async fn every_accepted_credential_and_shape_is_stripped_by_every_forward_site() {
    for fixture in fixtures() {
        let state = &fixture.state;
        let shapes = delivery_shapes(state);
        let mut accepted = 0usize;

        for kind in credential_kinds() {
            for shape in &shapes {
                let headers = request_headers(shape, &kind.value);
                if !accepted_by_any_gate(state, &headers) {
                    continue;
                }
                accepted += 1;
                for site in FORWARD_SITES {
                    let out = site.run(state, &headers).await;
                    assert!(
                        !contains_value(&out, &kind.value),
                        "{} forwarded an accepted credential: config={}, kind={}, shape={}",
                        site.name(),
                        fixture.name,
                        kind.name,
                        shape.name,
                    );
                }
            }
        }

        assert_eq!(
            accepted, fixture.accepted_pairs,
            "the accept predicates admitted a different number of (kind, shape) pairs than the \
             {:?} fixture expects; update its `accepted_pairs` *and* re-check that every newly \
             accepted pair is stripped",
            fixture.name
        );
    }
}

#[tokio::test]
async fn no_forward_site_relays_any_shunt_credential_in_any_shape() {
    // Stronger than the mirror invariant, and deliberately so: the strip
    // predicates are allowed to over-strip, and today they do — a shunt-shaped
    // JWT that no longer verifies authenticates nothing yet must still never
    // reach a third party. Recording the stronger property here means a future
    // change that narrows a strip predicate back down to "exactly what is
    // accepted" is a visible decision rather than a silent regression.
    for fixture in fixtures() {
        let state = &fixture.state;
        let shapes = delivery_shapes(state);

        for kind in credential_kinds() {
            for shape in &shapes {
                let headers = request_headers(shape, &kind.value);
                for site in FORWARD_SITES {
                    let out = site.run(state, &headers).await;
                    assert!(
                        !contains_value(&out, &kind.value),
                        "{} forwarded a shunt credential: config={}, kind={}, shape={}",
                        site.name(),
                        fixture.name,
                        kind.name,
                        shape.name,
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn every_forward_site_produces_a_non_empty_result_for_a_benign_request() {
    // Non-vacuity guard for the two tests above: if a site returned an empty
    // map for everything, "the credential is absent" would hold trivially. The
    // benign request carries the caller's own upstream key, because
    // `discovery::upstream` legitimately answers `None` when no forwardable
    // credential is left.
    let state = state();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("x-api-key", GENUINE_UPSTREAM_KEY.parse().unwrap());

    for site in FORWARD_SITES {
        let out = site.run(&state, &headers).await;
        assert!(
            !out.is_empty(),
            "{} produced an empty header map for a benign request",
            site.name()
        );
    }
}

#[tokio::test]
async fn a_genuine_upstream_credential_survives_beside_a_consumed_one() {
    // The per-slot rule, at each site that has one: a gateway JWT in
    // `Authorization` must not take the caller's real key in `x-api-key` down
    // with it. This is what a request-level (rather than per-slot) strip
    // decision cannot express.
    let state = state();
    let jwt = gateway_jwt(GATEWAY_URL);
    let mut headers = HeaderMap::new();
    headers.insert("authorization", bearer(&jwt).parse().unwrap());
    headers.insert("x-api-key", GENUINE_UPSTREAM_KEY.parse().unwrap());

    for site in [
        ForwardSite::InferenceFailover,
        ForwardSite::DiscoveryPassthrough,
    ] {
        let out = site.run(&state, &headers).await;
        assert!(
            out.get("authorization").is_none(),
            "{} kept the consumed slot",
            site.name()
        );
        assert_eq!(
            out.get("x-api-key").map(HeaderValue::as_bytes),
            Some(GENUINE_UPSTREAM_KEY.as_bytes()),
            "{} dropped the caller's own upstream credential",
            site.name()
        );
    }

    // Site 3 is different by design and the difference is recorded rather than
    // hidden: `[server.codex_endpoint]` targets a validated `chatgpt_oauth`
    // backend, so no inbound `authorization`/`x-api-key` value can ever be a
    // valid upstream credential there and both shared slots go unconditionally.
    let out = ForwardSite::CodexPassthrough.run(&state, &headers).await;
    assert!(out.get("authorization").is_none());
    assert!(out.get("x-api-key").is_none());
}

// --- Multi-value slots (#392) ------------------------------------------------
//
// A `HeaderMap` slot is a *list*, not a scalar. `append` puts a second value on
// the same name, and forward site 1 starts from a clone of the caller's map, so
// every value survives to the upstream. The strip predicates read only
// `HeaderMap::get`, which returns the **first** value — so a shunt credential
// sitting behind a genuine one in the same slot is judged as "not consumed" and
// relayed. Sites 2 and 3 are structurally immune: site 2 copies one value per
// slot into a fresh map, site 3 drops both shared slots unconditionally.

/// `x-api-key: <genuine>` followed by `x-api-key: <admin credential>`.
fn multi_value_api_key(second: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.append("x-api-key", GENUINE_UPSTREAM_KEY.parse().unwrap());
    headers.append("x-api-key", second.parse().unwrap());
    headers
}

#[tokio::test]
async fn a_shunt_credential_in_a_trailing_x_api_key_value_is_not_forwarded() {
    let state = state();
    let headers = multi_value_api_key(ADMIN_WRITE_KEY);

    let out = ForwardSite::InferenceFailover.run(&state, &headers).await;

    assert!(
        !contains_value(&out, ADMIN_WRITE_KEY),
        "the inference passthrough relayed an admin credential riding in the second \
         `x-api-key` value"
    );
    // The implemented behavior is whole-slot removal, so the genuine credential
    // sharing the slot goes too. Asserted explicitly: this is over-stripping,
    // which the invariant permits, and recording it here keeps it a decision
    // rather than something a future reader meets as a bug report.
    assert!(
        out.get("x-api-key").is_none(),
        "a consumed value must take the whole slot with it"
    );
}

#[tokio::test]
async fn a_multi_value_x_api_key_with_no_shunt_credential_survives_intact() {
    // Non-vacuity control for the two tests above. Without it, a strip that
    // simply dropped every multi-value slot would satisfy them. Two genuine
    // caller values must both still reach the upstream.
    let state = state();
    let second = "sk-ant-second-genuine-key";
    let mut headers = HeaderMap::new();
    headers.append("x-api-key", GENUINE_UPSTREAM_KEY.parse().unwrap());
    headers.append("x-api-key", second.parse().unwrap());

    let out = ForwardSite::InferenceFailover.run(&state, &headers).await;

    let values: Vec<&[u8]> = out
        .get_all("x-api-key")
        .iter()
        .map(HeaderValue::as_bytes)
        .collect();
    assert_eq!(
        values,
        vec![GENUINE_UPSTREAM_KEY.as_bytes(), second.as_bytes()],
        "both of the caller's own values must survive, in order"
    );
}

#[tokio::test]
async fn a_shunt_credential_in_the_leading_x_api_key_value_is_still_not_forwarded() {
    // The pre-#392 ordering, kept as a regression guard: widening the scan to
    // every value must not stop it finding one in the position `get` already
    // returned.
    let state = state();
    let mut headers = HeaderMap::new();
    headers.append("x-api-key", ADMIN_WRITE_KEY.parse().unwrap());
    headers.append("x-api-key", GENUINE_UPSTREAM_KEY.parse().unwrap());

    let out = ForwardSite::InferenceFailover.run(&state, &headers).await;

    assert!(!contains_value(&out, ADMIN_WRITE_KEY));
    assert!(out.get("x-api-key").is_none());
}

#[tokio::test]
async fn a_shunt_credential_in_a_trailing_authorization_value_is_not_forwarded() {
    // Both shapes, on the trailing value, because each value has to be judged
    // as a `Bearer` payload *and* as a whole raw value (#361). A multi-value
    // loop that keeps only one of the two shapes passes half of this test.
    let state = state();
    let jwt = gateway_jwt(GATEWAY_URL);

    for (shape, second) in [("bearer", bearer(&jwt)), ("raw", jwt.clone())] {
        let mut headers = HeaderMap::new();
        headers.append(
            "authorization",
            bearer(GENUINE_UPSTREAM_KEY).parse().unwrap(),
        );
        headers.append("authorization", second.parse().unwrap());

        let out = ForwardSite::InferenceFailover.run(&state, &headers).await;

        assert!(
            !contains_value(&out, &jwt),
            "the inference passthrough relayed a gateway JWT riding in the second \
             `authorization` value ({shape} shape)"
        );
    }
}

/// A configured `[server.auth]` token whose **whole value** begins with a
/// `Bearer ` scheme. `parse_tokens` keeps everything after the first `:` and
/// preserves inner whitespace, so `SHUNT_CLIENT_TOKENS="ci:Bearer <secret>"` is
/// a legal deployment.
const SCHEME_SHAPED_TOKEN: &str = "Bearer scheme-shaped-static-token";

#[tokio::test]
async fn both_authorization_shapes_are_judged_for_the_same_value() {
    // Pins the *structure* of the per-value check, which nothing else in this
    // file does. With `[server.auth] header = "authorization"` and a token that
    // itself starts with `Bearer `, the two shapes disagree about the same
    // value: the payload (`scheme-shaped-static-token`) matches no credential,
    // while the whole raw value is exactly the configured token, so
    // `authenticate` accepts the request. A strip that resolves the shapes with
    // `unwrap_or_else` — take the payload, else the raw value — checks only the
    // payload here, finds nothing, and relays a credential shunt just
    // authenticated. Both shapes have to be tried for every value, not one per
    // value.
    let mut state = state_with_headers(
        HeaderName::from_static("authorization"),
        HeaderName::from_static("x-shunt-admin-token"),
    );
    state.inbound_auth = Some(Arc::new(InboundAuth::new(
        HeaderName::from_static("authorization"),
        vec![("ci".to_string(), SCHEME_SHAPED_TOKEN.to_string())],
    )));

    let mut headers = HeaderMap::new();
    headers.insert("authorization", SCHEME_SHAPED_TOKEN.parse().unwrap());

    // Non-vacuity: the real accept predicate must actually admit this, or the
    // mirror invariant would not require anything of the strip side.
    assert!(
        accepted_by_any_gate(&state, &headers),
        "fixture no longer authenticates; the assertion below would be vacuous"
    );

    for site in FORWARD_SITES {
        let out = site.run(&state, &headers).await;
        assert!(
            !contains_value(&out, SCHEME_SHAPED_TOKEN),
            "{} relayed a static token whose whole value carries a Bearer scheme",
            site.name()
        );
    }
}

#[tokio::test]
async fn the_codex_passthrough_never_relays_an_admin_credential_header() {
    // Regression for the gap this module closed: `[server.admin]`'s header was
    // absent from the Codex strip list, so an admin credential — the highest
    // value one shunt holds, since it can provision upstream accounts — was
    // relayed verbatim to the ChatGPT backend.
    //
    // Both the default name and a custom configured one are covered: the
    // default is stripped unconditionally (it is never a legitimate upstream
    // header), the configured one because `strip_reserved_slots` reads it off
    // the live `[server.admin]` config.
    let default_header = state();
    let custom_header = state_with_admin_header(HeaderName::from_static("x-corp-admin"));

    for (state, header) in [
        (&default_header, "x-shunt-admin-token"),
        (&custom_header, "x-corp-admin"),
        // The default reserved name is stripped even when the operator moved
        // the admin header elsewhere.
        (&custom_header, "x-shunt-admin-token"),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(header, ADMIN_WRITE_KEY.parse().unwrap());
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let out = ForwardSite::CodexPassthrough.run(state, &headers).await;

        assert!(
            !contains_value(&out, ADMIN_WRITE_KEY),
            "the codex passthrough relayed an admin credential presented in {header}"
        );
        // Non-vacuity: the map is not empty, so absence means "removed", not
        // "nothing was built".
        assert!(out.get("content-type").is_some());
    }
}

#[tokio::test]
async fn no_forward_site_relays_the_admin_session_cookie() {
    // `admin::authenticate` falls back to `session_cookie`, which accepts a
    // write-tier `shunt_admin_session` out of the `cookie` header when no
    // credential header matched. That made `cookie` an accept slot the first
    // version of this enumeration missed, and two of the three forward sites
    // relayed the header verbatim: `headers_for_route` starts from
    // `base.clone()` on both branches, and the Codex strip list had no `cookie`
    // entry. Only `discovery::upstream` was safe, and only incidentally —
    // it builds its map from scratch.
    let state = state();
    let mut headers = HeaderMap::new();
    headers.insert(
        "cookie",
        "shunt_admin_session=sid-abc123; theme=dark"
            .parse()
            .unwrap(),
    );
    headers.insert("x-api-key", GENUINE_UPSTREAM_KEY.parse().unwrap());

    for site in FORWARD_SITES {
        let out = site.run(&state, &headers).await;
        assert!(
            !contains_value(&out, "sid-abc123"),
            "{} relayed the admin session cookie",
            site.name()
        );
        // The behavior implemented is a **whole-header** strip, not a surgical
        // removal of the `shunt_admin_session=` pair. Assert the over-strip
        // explicitly so it is a recorded decision rather than something a
        // future reader discovers from a bug report: a benign cookie a caller
        // sent is dropped too. A surgical parser was rejected because it would
        // have to track `session_cookie`'s own parse and would reintroduce the
        // accept/strip drift this module exists to eliminate.
        assert!(
            out.get("cookie").is_none(),
            "{} kept the cookie header; the implemented behavior is whole-header removal",
            site.name()
        );
    }
}

#[tokio::test]
async fn stripping_the_cookie_header_leaves_the_callers_own_credential_alone() {
    // Non-vacuity control for the test above: the `cookie` strip is by name and
    // must not disturb the by-value decision on the shared slots. Sites 1 and 2
    // keep the caller's genuine upstream key; site 3 drops both shared slots
    // unconditionally by design, which is asserted separately.
    let state = state();
    let mut headers = HeaderMap::new();
    headers.insert("cookie", "theme=dark".parse().unwrap());
    headers.insert("x-api-key", GENUINE_UPSTREAM_KEY.parse().unwrap());

    for site in [
        ForwardSite::InferenceFailover,
        ForwardSite::DiscoveryPassthrough,
    ] {
        let out = site.run(&state, &headers).await;
        assert_eq!(
            out.get("x-api-key").map(HeaderValue::as_bytes),
            Some(GENUINE_UPSTREAM_KEY.as_bytes()),
            "{} dropped the caller's own upstream credential",
            site.name()
        );
        assert!(out.get("cookie").is_none());
    }
}

// --- Tripwire ----------------------------------------------------------------

/// Files allowed to produce or bulk-apply an outbound header map.
///
/// A file lands here by either building a `HeaderMap` that something sends, or
/// applying one wholesale to a request. Each is one of three things — a
/// registered forward site (which must also appear in [`FORWARD_SITES`] with a
/// row in the mirror table above), an allowlist-built map that never carries a
/// caller header, or test scaffolding. Being none of those, and unclassified,
/// is what this catches.
///
/// The type-signature half of the scan is what makes it worth having. Matching
/// only the bulk-application idioms left **both** hand-rolled forward sites
/// invisible: `discovery/upstream.rs` feeds its map to `request.header(k, v)`
/// in a loop, and `proxy/failover.rs` returns `headers.clone()`/`base.clone()`.
/// A new forward site written by copying either one escaped detection.
///
/// Residual hole, narrower than the bulk-only version but still real: a site
/// that mutates a request in place and never returns a `HeaderMap` is caught
/// only by the extend-into-`headers_mut` pattern, so a different in-place idiom
/// would slip through. Files named `tests.rs` are skipped so fixtures need no
/// entry; an in-file `#[cfg(test)] mod tests` helper is *not* skipped and is
/// listed below as noise.
const HEADER_PRODUCER_ALLOWLIST: [&str; 9] = [
    // noise — `#[cfg(test)] mod tests` fixture builder.
    "src/accounts.rs",
    // registered forward site — consumes the map `headers_for_route` produced
    // (site 1) and adds only the resolved provider credential.
    "src/adapters/anthropic/mod.rs",
    // allowlist-built, not a forward site — the Codex identity and beta headers
    // are synthesized, never copied from the caller.
    "src/adapters/responses/codex_ws.rs",
    // registered forward site — site 3, the inbound Codex passthrough.
    "src/adapters/responses/inbound.rs",
    // noise — `#[cfg(test)] mod tests` fixture builder.
    "src/admin/mod.rs",
    // registered forward site — site 2, discovery's passthrough branch.
    "src/discovery/upstream.rs",
    // allowlist-built, not a forward site — the OTLP relay forwards only
    // content-type/content-encoding beside the destination's configured headers.
    "src/gateway/telemetry_ingest.rs",
    // header-derivation site, not a forward site on its own — `filtered()` drops
    // hop-by-hop names from a map its callers already stripped credentials from.
    "src/headers.rs",
    // registered forward site — site 1, `check_inbound_auth` + `headers_for_route`.
    "src/proxy/failover.rs",
];

#[test]
fn every_header_producing_site_is_classified() {
    // Patterns are assembled rather than written literally so this file does
    // not match itself.
    let bulk_apply = format!(".{}(", "headers");
    let bulk_apply_getter = format!(".{}()", "headers");
    let bulk_extend = format!("{}_mut().{}(", "headers", "extend");
    let produces = format!("-> {}", "HeaderMap");
    let produces_optional = format!("-> Option<{}>", "HeaderMap");

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("src is readable") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("rust sources are UTF-8");
            let bulk = source
                .match_indices(&bulk_apply)
                .any(|(at, _)| !source[at..].starts_with(&bulk_apply_getter));
            // A dedicated `tests.rs` is scaffolding by construction, so a
            // fixture returning a `HeaderMap` does not need an allowlist entry.
            // Bulk application still counts there — a test that relays headers
            // is exercising a real path.
            let is_test_file = path.file_name().is_some_and(|name| name == "tests.rs");
            let produces_map = !is_test_file
                && (source.contains(&produces) || source.contains(&produces_optional));
            if bulk || source.contains(&bulk_extend) || produces_map {
                let relative = path.strip_prefix(&root).expect("scanned under src");
                // Join with `/` explicitly rather than using `Path::display`,
                // whose separator is platform-dependent: on Windows it renders
                // `auth\slots.rs`, which matches nothing in the
                // forward-slashed allowlist and fails the tripwire for a
                // reason that has nothing to do with a new forward site.
                // Building from components rather than replacing `\` keeps a
                // backslash that is a legitimate character in a Unix filename.
                let relative = relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                found.insert(format!("src/{relative}"));
            }
        }
    }

    let expected: BTreeSet<String> = HEADER_PRODUCER_ALLOWLIST
        .iter()
        .map(|path| (*path).to_string())
        .collect();
    assert_eq!(
        found, expected,
        "a file gained or lost an outbound header map. If it relays caller-supplied headers to a \
         third party, register it in FORWARD_SITES and extend the mirror table above; if it \
         builds its map from an allowlist, or is test scaffolding, add it to \
         HEADER_PRODUCER_ALLOWLIST with a comment saying which."
    );
}
