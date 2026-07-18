pub mod approval;
mod device;
mod idp;
mod idp_client;
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
pub struct ResolvedIdp {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub allowed_domains: Vec<String>,
    pub allowed_emails: Vec<String>,
    pub scopes: Vec<String>,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub userinfo_endpoint: Option<String>,
}

impl ResolvedIdp {
    pub fn button_label(&self) -> &'static str {
        if reqwest::Url::parse(&self.issuer)
            .ok()
            .and_then(|url| url.host_str().map(ToOwned::to_owned))
            .as_deref()
            == Some("accounts.google.com")
        {
            "Sign in with Google"
        } else {
            "Sign in with SSO"
        }
    }

    pub fn email_allowed(&self, email: &str) -> bool {
        let email = email.to_ascii_lowercase();
        self.allowed_emails.iter().any(|allowed| allowed == &email)
            || email.rsplit_once('@').is_some_and(|(_, domain)| {
                self.allowed_domains.iter().any(|allowed| allowed == domain)
            })
    }
}

#[derive(Clone)]
pub struct GatewayAuth {
    public_url: String,
    jwt_secret: Vec<u8>,
    token_ttl_seconds: u64,
    trust_forwarded_for: bool,
    approval: Option<Arc<dyn ApprovalProvider>>,
    oidc: Option<Arc<ResolvedIdp>>,
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
        Self::with_optional_approval(
            public_url,
            jwt_secret,
            token_ttl_seconds,
            trust_forwarded_for,
            Some(approval),
        )
    }

    pub(crate) fn with_optional_approval(
        public_url: String,
        jwt_secret: Vec<u8>,
        token_ttl_seconds: u64,
        trust_forwarded_for: bool,
        approval: Option<Arc<dyn ApprovalProvider>>,
    ) -> Self {
        Self {
            public_url,
            jwt_secret,
            token_ttl_seconds,
            trust_forwarded_for,
            approval,
            oidc: None,
            managed_default: None,
            managed_by_email: HashMap::new(),
        }
    }

    pub fn with_oidc(mut self, idp: ResolvedIdp) -> Self {
        self.oidc = Some(Arc::new(idp));
        self
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

    pub fn approval_provider(&self) -> Option<&dyn ApprovalProvider> {
        self.approval.as_deref()
    }

    pub fn oidc(&self) -> Option<&ResolvedIdp> {
        self.oidc.as_deref()
    }

    pub(crate) fn oidc_arc(&self) -> Option<Arc<ResolvedIdp>> {
        self.oidc.clone()
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
        .route("/device/authorize", post(idp::authorize))
        .route("/device/callback", get(idp::callback))
        .route("/managed/settings", get(managed::get))
}

#[cfg(test)]
mod tests;
