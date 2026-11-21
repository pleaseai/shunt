//! A token issued by `POST /v1/oauth/token`, presented to the routes the
//! inbound gate guards, end to end: a real router, a loopback JWKS issuer and
//! mock upstreams. Every token here comes from the real endpoint.
//!
//! The properties pinned: the token authenticates as the identity of the entry
//! that verified the assertion (so spend is metered under it), only from
//! `Authorization: Bearer`, never ahead of a static token, and not on the
//! routes that do not call the gate (admin, spend API, telemetry ingest).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{
        AdminConfig, AuthMode, Config, ExchangeConfig, GatewayConfig, PricingConfig,
        PricingOverride, SpendConfig, UsageEndpointConfig,
    },
    gateway::spend::{
        meter::{window, PERIODS},
        pricing::{Rates, Usage},
    },
};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::common::{
    self,
    codex_jwt::{self, Gateway, JWT_COOKIE, JWT_HEADER},
    jwt,
};

const GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const BETA: &str = "oauth-2025-04-20";
const IDENTITY: &str = "alice";
const KEY_ENV: &str = "SHUNT_TEST_EXCHANGE_GATE_KEY";
const STATIC_ENV: &str = "SHUNT_TEST_EXCHANGE_GATE_STATIC";
const GATEWAY_JWT_ENV: &str = "SHUNT_TEST_EXCHANGE_GATE_GW_JWT";
const GATEWAY_USERS_ENV: &str = "SHUNT_TEST_EXCHANGE_GATE_GW_USERS";
const RATES: (f64, f64, f64, f64) = (3.0, 15.0, 0.3, 3.75);
const MESSAGES_BODY: &str =
    r#"{"model":"upstream-billed","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;
const RESPONSES_BODY: &str = r#"{"model":"gpt-5.6-sol","input":[],"stream":false,"store":false}"#;

struct Fixture {
    gateway: Gateway,
    _issuer: MockServer,
    _anthropic: MockServer,
    _codex: MockServer,
    /// The assertion the sidecar would trade, for `IDENTITY`.
    assertion: String,
    _env: common::EnvVars,
}

fn usage() -> Usage {
    Usage {
        input_tokens: 1000,
        output_tokens: 500,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    }
}

fn expected_cost() -> u64 {
    Rates::from_usd_per_million(RATES.0, RATES.1, RATES.2, RATES.3).cost_femto_usd(&usage())
}

async fn anthropic_upstream() -> MockServer {
    let server = MockServer::start().await;
    let reply = json!({"id": "msg_1", "type": "message", "model": "upstream-billed",
        "content": [{"type": "text", "text": "Hello"}],
        "usage": {"input_tokens": 1000, "output_tokens": 500}});
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 5})))
        .mount(&server)
        .await;
    server
}

/// One gateway carrying every gated surface plus the ungated ones the exchange
/// token must not reach. A static client `bob` exists beside the exchange.
async fn fixture() -> Fixture {
    let env = common::set_env(&[
        (codex_jwt::ACCOUNT_ENV, &codex_jwt::chatgpt_token()),
        (codex_jwt::ROUTE_KEY_ENV, "route-key"),
        (codex_jwt::ADMIN_ENV, "admin:admin-secret"),
        (KEY_ENV, "upstream-key"),
        (STATIC_ENV, "bob:static-token"),
        (GATEWAY_JWT_ENV, "0123456789abcdef0123456789abcdef"),
        (GATEWAY_USERS_ENV, "dev@example.com:password"),
    ])
    .await;
    let issuer = jwt::issuer().await;
    let anthropic = anthropic_upstream().await;
    let codex = codex_jwt::upstream("/codex/responses").await;

    let mut auth = codex_jwt::auth(
        &issuer.uri(),
        &format!("{}/jwks", issuer.uri()),
        Some(JWT_HEADER),
        Some(JWT_COOKIE),
    );
    auth.tokens_env = STATIC_ENV.to_string();
    auth.jwt[0].exchange = true;
    auth.exchange = Some(
        serde_json::from_value::<ExchangeConfig>(
            json!({"signing_secret": "exchange-gate-secret-0123456789abcdef"}),
        )
        .unwrap(),
    );
    let mut config: Config = codex_jwt::config(&codex.uri(), auth);

    let anthropic_provider = config.providers.get_mut("anthropic").unwrap();
    anthropic_provider.base_url = anthropic.uri();
    anthropic_provider.auth = AuthMode::ApiKey;
    anthropic_provider.api_key_env = Some(KEY_ENV.to_string());
    config.server.admin = Some(AdminConfig {
        header: "x-shunt-admin-token".to_string(),
        tokens_env: codex_jwt::ADMIN_ENV.to_string(),
        tokens_file: None,
        write_keys: Vec::new(),
        read_keys: Vec::new(),
        session_ttl_secs: 3600,
        pending_ttl_secs: 600,
        hide_observed: false,
        oidc: None,
    });
    config.server.spend = Some(SpendConfig {
        state_path: Some(std::path::PathBuf::new()),
        pricing: Some(PricingConfig {
            multiplier: 1.0,
            overrides: vec![PricingOverride {
                upstream: "anthropic".to_string(),
                model: "upstream-billed".to_string(),
                input: RATES.0,
                output: RATES.1,
                cache_read: RATES.2,
                cache_write: RATES.3,
            }],
        }),
        ..SpendConfig::default()
    });
    config.server.usage = Some(UsageEndpointConfig::default());
    config.server.gateway = Some(GatewayConfig {
        desktop_bootstrap: None,
        public_url: "https://gateway.example".to_string(),
        jwt_secret_env: Some(GATEWAY_JWT_ENV.to_string()),
        users_env: GATEWAY_USERS_ENV.to_string(),
        token_ttl_seconds: Some(3600),
        trust_forwarded_for: false,
        policies: None,
        telemetry: None,
        state_path: Some(std::path::PathBuf::new()),
        oidc: None,
        session: None,
    });
    let assertion = codex_jwt::jwt_for(&issuer.uri(), IDENTITY, 300);
    Fixture {
        gateway: codex_jwt::start(config.validate().expect("fixture config is valid")).await,
        _issuer: issuer,
        _anthropic: anthropic,
        _codex: codex,
        assertion,
        _env: env,
    }
}

impl Fixture {
    /// Trade the assertion at the real endpoint.
    async fn issue(&self) -> String {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/oauth/token", self.gateway.base_url))
            .header("anthropic-beta", BETA)
            .json(&json!({"grant_type": GRANT, "assertion": self.assertion}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        body["access_token"].as_str().unwrap().to_string()
    }

    async fn send(
        &self,
        http_method: &str,
        route: &str,
        body: Option<&str>,
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .request(
                http_method.parse().unwrap(),
                format!("{}{route}", self.gateway.base_url),
            )
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = body {
            request = request.body(body.to_string());
        }
        request.send().await.unwrap()
    }

    /// `principal`'s spend in each period, summing both windows across a
    /// boundary that fell during the test.
    fn spent(&self, principal: &str) -> [u64; 3] {
        let meter = self.gateway.state.gateway_stores.spend.meter();
        let now = codex_jwt::now();
        PERIODS.map(|period| {
            let current = meter.spent(principal, period, now);
            if window(period, now).start == window(period, now - 120).start {
                current
            } else {
                current + meter.spent(principal, period, now - 120)
            }
        })
    }
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

#[tokio::test]
async fn an_issued_token_authenticates_every_gated_route_as_the_verifying_identity() {
    let fixture = fixture().await;
    let token = fixture.issue().await;
    let authorization = bearer(&token);
    let headers = [
        ("authorization", authorization.as_str()),
        ("anthropic-beta", BETA),
    ];

    let routes: [(&str, &str, Option<&str>); 5] = [
        ("POST", "/v1/messages", Some(MESSAGES_BODY)),
        ("POST", "/v1/messages/count_tokens", Some(MESSAGES_BODY)),
        ("GET", "/v1/models", None),
        ("GET", "/usage", None),
        ("POST", "/v1/responses", Some(RESPONSES_BODY)),
    ];
    for (http_method, route, body) in routes {
        let response = fixture.send(http_method, route, body, &headers).await;
        assert_eq!(response.status(), StatusCode::OK, "{http_method} {route}");
        response.bytes().await.unwrap();
    }
}

#[tokio::test]
async fn spend_is_metered_under_the_identity_the_token_carries() {
    let fixture = fixture().await;
    let token = fixture.issue().await;

    let response = fixture
        .send(
            "POST",
            "/v1/messages",
            Some(MESSAGES_BODY),
            &[("authorization", &bearer(&token)), ("anthropic-beta", BETA)],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();

    let cost = expected_cost();
    assert!(cost > 0);
    assert_eq!(fixture.spent(IDENTITY), [cost; 3]);
    assert_eq!(
        fixture.spent("bob"),
        [0; 3],
        "the static client is untouched"
    );
}

#[tokio::test]
async fn a_cap_on_the_identity_refuses_its_exchange_token() {
    let fixture = fixture().await;
    let token = fixture.issue().await;
    let created = fixture
        .send(
            "POST",
            "/v1/organizations/spend_limits",
            Some(
                &json!({"amount": "100",
                        "scope": {"type": "user", "user_id": IDENTITY},
                        "period": "daily"})
                .to_string(),
            ),
            &[("x-shunt-admin-token", "admin-secret")],
        )
        .await;
    assert_eq!(created.status(), StatusCode::OK, "cap must be created");
    let remaining = window(
        shunt::gateway::spend::store::Period::Daily,
        codex_jwt::now(),
    )
    .end
    .saturating_sub(codex_jwt::now());
    if remaining < 5 {
        tokio::time::sleep(std::time::Duration::from_secs(remaining + 1)).await;
    }
    fixture.gateway.state.gateway_stores.spend.meter().record(
        IDENTITY,
        codex_jwt::now(),
        100 * shunt::gateway::spend::meter::FEMTO_USD_PER_CENT as u64,
    );

    let response = fixture
        .send(
            "POST",
            "/v1/messages",
            Some(MESSAGES_BODY),
            &[("authorization", &bearer(&token)), ("anthropic-beta", BETA)],
        )
        .await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn the_token_does_not_authenticate_outside_the_bearer_slot() {
    let fixture = fixture().await;
    let token = fixture.issue().await;
    let cookie = format!("{JWT_COOKIE}={token}");

    let elsewhere: [(&str, &str); 4] = [
        ("x-api-key", &token),
        ("x-shunt-token", &token),
        (JWT_HEADER, &token),
        ("cookie", &cookie),
    ];
    for (name, value) in elsewhere {
        let messages = fixture
            .send(
                "POST",
                "/v1/messages",
                Some(MESSAGES_BODY),
                &[(name, value), ("anthropic-beta", BETA)],
            )
            .await;
        assert_eq!(messages.status(), StatusCode::UNAUTHORIZED, "{name}");
        let codex = fixture
            .send(
                "POST",
                "/v1/responses",
                Some(RESPONSES_BODY),
                &[(name, value)],
            )
            .await;
        assert_eq!(codex.status(), StatusCode::UNAUTHORIZED, "codex {name}");
    }
}

#[tokio::test]
async fn a_static_token_beside_an_exchange_token_authenticates_as_the_static_client() {
    let fixture = fixture().await;
    let token = fixture.issue().await;

    let response = fixture
        .send(
            "POST",
            "/v1/messages",
            Some(MESSAGES_BODY),
            &[
                ("x-shunt-token", "static-token"),
                ("authorization", &bearer(&token)),
                ("anthropic-beta", BETA),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();

    assert_eq!(fixture.spent("bob"), [expected_cost(); 3]);
    assert_eq!(fixture.spent(IDENTITY), [0; 3]);
}

#[tokio::test]
async fn admin_spend_api_and_telemetry_reject_the_token_like_any_other_credential() {
    let fixture = fixture().await;
    let token = fixture.issue().await;
    let authorization = bearer(&token);

    for (http_method, route, body) in [
        ("GET", "/admin/api/accounts", None),
        ("GET", "/v1/organizations/spend_limits", None),
        ("POST", "/v1/metrics", Some("{}")),
    ] {
        let with_token = fixture
            .send(
                http_method,
                route,
                body,
                &[("authorization", &authorization), ("anthropic-beta", BETA)],
            )
            .await;
        let with_junk = fixture
            .send(
                http_method,
                route,
                body,
                &[("authorization", "Bearer not-a-credential")],
            )
            .await;
        assert!(
            with_token.status().is_client_error(),
            "{route}: {}",
            with_token.status()
        );
        assert_eq!(with_token.status(), with_junk.status(), "{route}");
    }
}
