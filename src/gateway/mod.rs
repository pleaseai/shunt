pub mod approval;
mod device;
pub mod jwt;
pub mod managed;
mod oauth;
pub mod persist;
pub mod refresh;
pub mod store;

use std::{collections::HashMap, sync::Arc};

use axum::{
    http::HeaderMap,
    routing::{get, post},
    Router,
};
use serde_json::Value;

use crate::server::AppState;

pub use store::GatewayStores;

use approval::{ApprovalProvider, StaticUsers};
use managed::ResolvedPolicy;

#[derive(Clone)]
pub struct GatewayAuth {
    public_url: String,
    jwt_secret: Vec<u8>,
    token_ttl_seconds: u64,
    trust_forwarded_for: bool,
    approval: Arc<dyn ApprovalProvider>,
    managed_default: Option<Value>,
    managed_by_email: HashMap<String, Value>,
}

impl GatewayAuth {
    pub fn new(
        public_url: String,
        jwt_secret: Vec<u8>,
        token_ttl_seconds: u64,
        trust_forwarded_for: bool,
        users: StaticUsers,
    ) -> Self {
        Self::with_approval_provider(
            public_url,
            jwt_secret,
            token_ttl_seconds,
            trust_forwarded_for,
            Arc::new(users),
        )
    }

    pub fn with_approval_provider(
        public_url: String,
        jwt_secret: Vec<u8>,
        token_ttl_seconds: u64,
        trust_forwarded_for: bool,
        approval: Arc<dyn ApprovalProvider>,
    ) -> Self {
        Self {
            public_url,
            jwt_secret,
            token_ttl_seconds,
            trust_forwarded_for,
            approval,
            managed_default: None,
            managed_by_email: HashMap::new(),
        }
    }

    pub(crate) fn with_managed_policies(
        mut self,
        policies: Option<Vec<ResolvedPolicy>>,
        telemetry_push: bool,
    ) -> Self {
        if let Some(policies) = policies {
            let (managed_default, managed_by_email) =
                managed::resolve_all(&policies, telemetry_push, &self.public_url);
            self.managed_default = Some(managed_default);
            self.managed_by_email = managed_by_email;
        }
        self
    }

    pub(crate) fn managed_settings(&self, email: &str) -> Option<&Value> {
        self.managed_by_email
            .get(email)
            .or(self.managed_default.as_ref())
    }

    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    pub fn jwt_secret(&self) -> &[u8] {
        &self.jwt_secret
    }

    pub fn token_ttl_seconds(&self) -> u64 {
        self.token_ttl_seconds
    }

    pub fn trust_forwarded_for(&self) -> bool {
        self.trust_forwarded_for
    }

    pub fn approval_provider(&self) -> &dyn ApprovalProvider {
        self.approval.as_ref()
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.public_url.trim_end_matches('/'), path)
    }

    pub fn authenticate_bearer(&self, headers: &HeaderMap) -> Option<jwt::Claims> {
        let token = headers
            .get("authorization")?
            .to_str()
            .ok()?
            .trim()
            .split_once(' ')
            .and_then(|(scheme, token)| {
                scheme
                    .eq_ignore_ascii_case("bearer")
                    .then_some(token.trim())
            })?;
        jwt::verify(token, &self.public_url, &self.jwt_secret)
    }
}

pub fn gateway_router() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::discovery),
        )
        .route(
            "/oauth/device_authorization",
            post(oauth::device_authorization),
        )
        .route("/oauth/token", post(oauth::token))
        .route("/device", get(device::get).post(device::post))
        .route("/managed/settings", get(managed::get))
}

#[cfg(test)]
mod tests;
