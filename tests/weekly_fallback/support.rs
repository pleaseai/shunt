use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::Router;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use shunt::{
    accounts::{StoreFamily, UsageSnapshot, UsageWindow},
    config::{AccountConfig, AuthMode, Config, InboundAuthConfig, RouteConfig},
    server::{self, AppState},
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

pub const PAIRS: [(&str, &str); 4] = [
    ("claude-fable-5", "gpt-6-astra"),
    ("claude-opus-5", "gpt-5.6-sol"),
    ("claude-sonnet-5", "gpt-5.6-terra"),
    ("claude-haiku-4-5-20251001", "gpt-5.6-luna"),
];
pub const ALIAS: &str = "weekly-alias[1M]";
pub const CLIENT_TOKEN: &str = "synthetic-weekly-client";
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

pub struct Fixture {
    pub claude: MockServer,
    pub codex: MockServer,
    pub config: Config,
    /// Holds the shared environment lock for the fixture's lifetime and restores
    /// every variable it touched on drop, so raw `setenv` never escapes the guard
    /// (`tests/common/mod.rs`, enforced by `tests/env_lock_coverage.rs`).
    env: crate::common::EnvVars,
}

impl Fixture {
    /// Set one more variable under the fixture's held environment lock.
    pub fn put_env(&mut self, name: &str, value: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.env.set(name, value);
        self
    }

    /// Remove a variable for the rest of the fixture's lifetime, under the lock.
    pub fn clear_env(&mut self, name: &str) -> &mut Self {
        self.env.unset(name);
        self
    }
}

impl Fixture {
    pub async fn new(pair: usize, from_claude: bool, account_count: usize) -> Self {
        let mut env = crate::common::env_lock().await;
        let absent = std::env::temp_dir().join(format!(
            "shunt-weekly-absent-{}-{}",
            std::process::id(),
            reset_time()
        ));
        assert!(!absent.exists());
        env.set("SHUNT_CLAUDE_ACCOUNTS_DIR", absent.join("claude"));
        env.set("SHUNT_CODEX_ACCOUNTS_DIR", absent.join("codex"));
        env.set("SHUNT_CLAUDE_TOKEN_URL", "http://127.0.0.1:9/token");
        env.set("SHUNT_CODEX_TOKEN_URL", "http://127.0.0.1:9/token");
        let claude = MockServer::start().await;
        let codex = MockServer::start().await;
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut config = Config::default();
        config.server.bind = "127.0.0.1:0".into();
        config.server.weekly_fallback = Some(
            serde_json::from_value(json!({
                "enabled": true,
                "claude_provider": "anthropic",
                "codex_provider": "codex",
                "models": PAIRS.iter().enumerate().map(|(index, (claude, codex))| {
                    let mut pair = json!({"claude": claude, "codex": codex});
                    if index == 0 { pair["claude_fallback"] = json!(PAIRS[1].0); }
                    pair
                }).collect::<Vec<_>>()
            }))
            .unwrap(),
        );
        for (provider_name, base_url, family, mode) in [
            (
                "anthropic",
                claude.uri(),
                StoreFamily::Claude,
                AuthMode::ClaudeOauth,
            ),
            (
                "codex",
                codex.uri(),
                StoreFamily::Chatgpt,
                AuthMode::ChatgptOauth,
            ),
        ] {
            let provider = config.providers.get_mut(provider_name).unwrap();
            provider.base_url = base_url;
            provider.auth = mode;
            provider.websocket = false;
            provider.request_compression = false;
            provider.retry.max_retries = 0;
            provider.accounts = (0..account_count)
                .map(|index| {
                    let variable = format!("SHUNT_WEEKLY_{id}_{provider_name}_{index}");
                    let uuid = format!("weekly-{id}-{provider_name}-{index}");
                    let token = if provider_name == "codex" {
                        let payload = json!({
                            "exp": 4_102_444_800_u64,
                            "https://api.openai.com/auth": {"chatgpt_account_id": uuid}
                        });
                        format!("x.{}.y", URL_SAFE_NO_PAD.encode(payload.to_string()))
                    } else {
                        format!("synthetic-claude-{id}-{index}")
                    };
                    env.set(&variable, token);
                    AccountConfig {
                        name: format!("{provider_name}-{index}"),
                        token_env: Some(variable),
                        uuid: Some(uuid),
                        store_family: Some(family),
                        ..Default::default()
                    }
                })
                .collect();
        }
        let auth_variable = format!("SHUNT_WEEKLY_{id}_CLIENTS");
        env.set(&auth_variable, format!("test:{CLIENT_TOKEN}"));
        config.server.auth = Some(InboundAuthConfig {
            header: "x-shunt-token".into(),
            tokens_env: auth_variable.clone(),
        });
        let (provider, backend) = if from_claude {
            ("anthropic", PAIRS[pair].0)
        } else {
            ("codex", PAIRS[pair].1)
        };
        config.routes.push(route(provider, backend));
        Self {
            claude,
            codex,
            config,
            env,
        }
    }

    pub async fn success(&self) {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(claude_success())
            .mount(&self.claude)
            .await;
        Mock::given(method("POST"))
            .and(path("/codex/responses"))
            .respond_with(codex_success())
            .mount(&self.codex)
            .await;
    }

    pub fn environment(&mut self, suffix: &str, value: &str) -> String {
        let variable = format!(
            "{}_{}",
            self.config.server.auth.as_ref().unwrap().tokens_env,
            suffix
        );
        self.env.set(&variable, value);
        variable
    }

    pub async fn gateway(&self) -> Gateway {
        Gateway::start(self.config.clone()).await
    }

    pub fn account(&self, provider: &str, index: usize) -> &AccountConfig {
        &self.config.providers[provider].accounts[index]
    }

    pub fn quota(&self, gateway: &Gateway, provider: &str, index: usize, utilization: f64) {
        gateway.state.accounts.note_usage(
            provider,
            self.account(provider, index),
            &UsageSnapshot {
                seven_day: Some(UsageWindow {
                    utilization,
                    resets_at: Some(reset_time()),
                }),
                ..Default::default()
            },
        );
    }

    pub fn exhaust(&self, gateway: &Gateway, provider: &str) {
        for index in 0..self.config.providers[provider].accounts.len() {
            self.quota(gateway, provider, index, 1.0);
        }
    }

    pub async fn hits(&self, provider: &str) -> Vec<wiremock::Request> {
        if provider == "anthropic" {
            self.claude.received_requests().await.unwrap()
        } else {
            self.codex.received_requests().await.unwrap()
        }
    }
}

pub struct Gateway {
    pub base_url: String,
    pub state: AppState,
    pub app: Router,
    task: JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Gateway {
    pub async fn start(config: Config) -> Self {
        let (app, _, state) = server::build_router(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let served = app.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, served).await.unwrap();
        });
        Self {
            base_url,
            state,
            app,
            task,
        }
    }

    pub async fn post(&self) -> reqwest::Response {
        self.post_body(request_body(false)).await
    }

    pub async fn post_body(&self, body: Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-shunt-token", CLIENT_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

pub fn route(provider: &str, backend: &str) -> RouteConfig {
    RouteConfig {
        model: "weekly-alias".into(),
        provider: provider.into(),
        upstream_model: Some(backend.into()),
        effort: None,
        service_tier: None,
    }
}

pub fn request_body(stream: bool) -> Value {
    json!({"model": ALIAS, "max_tokens": 32, "stream": stream,
        "messages": [{"role": "user", "content": "hello"}]})
}

pub fn reset_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600
}

pub fn claude_success() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"type": "message", "id": "msg_weekly",
        "role": "assistant", "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}}))
}

pub fn codex_success() -> ResponseTemplate {
    ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
        .set_body_string(concat!(
            "event: response.created\ndata: {\"response\":{\"id\":\"resp_weekly\"}}\n\n",
            "event: response.output_item.added\ndata: {\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"delta\":\"hello\"}\n\n",
            "event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
        ))
}

pub fn quota_failure(provider: &str, status: u16) -> ResponseTemplate {
    let response = ResponseTemplate::new(status).set_body_json(json!({
        "type": "error", "error": {"type": "rate_limit_error", "message": "synthetic quota"}
    }));
    if provider == "anthropic" {
        response
            .insert_header("anthropic-ratelimit-unified-7d-utilization", "1.0")
            .insert_header(
                "anthropic-ratelimit-unified-7d-reset",
                reset_time().to_string(),
            )
            .insert_header("anthropic-ratelimit-unified-7d-status", "rejected")
            .insert_header("anthropic-ratelimit-unified-status", "rejected")
    } else {
        response
            .insert_header("x-codex-secondary-window-minutes", "10080")
            .insert_header("x-codex-secondary-used-percent", "100")
            .insert_header("x-codex-secondary-reset-at", reset_time().to_string())
    }
}

pub fn assert_headers(response: &reqwest::Response, provider: &str, backend: &str) {
    assert_eq!(response.headers()["x-gateway-upstream"], provider);
    assert_eq!(response.headers()["x-gateway-model"], ALIAS);
    assert_eq!(response.headers()["x-gateway-upstream-model"], backend);
}

pub fn upstream_body(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}
