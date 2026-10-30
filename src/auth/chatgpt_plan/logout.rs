//! `shunt logout chatgpt-plan --name <account>`: sign out of ChatGPT.
//!
//! The refresh token is revoked at the issuer's discovered revocation endpoint
//! (RFC 7009), then the account file is cut down to `{version, client_id}` so a
//! later sign-in reuses the registration. The local clear happens whether or
//! not the revocation was confirmed: the user asked to be signed out, and a
//! gateway that is already running reads the account file per use, so it sees
//! the signed-out record on its next request with no IPC. Nothing secret is
//! ever printed.

use std::time::Duration;

use anyhow::Context;
use reqwest::{StatusCode, Url};

use super::{
    id_token::OPENAI_ISSUER,
    record::AccountRecord,
    store::{self, ACCOUNT_FILE_LOCK, CLI_LOCK_TIMEOUT},
};
use crate::auth::shared::{self, file_lock, token_refresh_client};

const REVOKE_ATTEMPTS: u32 = 3;
const REVOKE_BACKOFF: Duration = Duration::from_millis(500);
const REVOKE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the sign-out talks. Production uses the issuer constant; tests inject
/// a loopback mock. There is deliberately no environment override: a
/// redirectable issuer would hand the refresh token to an arbitrary host.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub issuer: String,
}

impl Endpoints {
    pub fn production() -> Self {
        Self {
            issuer: OPENAI_ISSUER.into(),
        }
    }
}

/// Knobs that tests turn; production uses [`Behavior::production`].
pub struct Behavior {
    /// Total tries for a request that fails on the network or with a 5xx.
    pub attempts: u32,
    /// Delay before the second try; doubles after each further failure.
    pub backoff: Duration,
    /// Accept an `http` revocation endpoint (a loopback mock). Production
    /// requires `https`.
    pub allow_http: bool,
    /// Deadline for one request, body included. Expiry counts as a network
    /// error: retried, then not confirmed.
    pub attempt_timeout: Duration,
}

impl Behavior {
    pub fn production() -> Self {
        Self {
            attempts: REVOKE_ATTEMPTS,
            backoff: REVOKE_BACKOFF,
            allow_http: false,
            attempt_timeout: REVOKE_ATTEMPT_TIMEOUT,
        }
    }
}

/// The non-secret result of a sign-out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The issuer answered 200 and the tokens are cleared.
    Confirmed { name: String },
    /// The tokens are cleared locally; the issuer did not confirm.
    NotConfirmed { name: String },
    /// There were no tokens to revoke.
    AlreadySignedOut { name: String },
}

impl Outcome {
    pub fn render(&self) -> String {
        match self {
            Self::Confirmed { name } => {
                format!("ChatGPT plan account {name:?} signed out; remote revocation confirmed.")
            }
            Self::NotConfirmed { name } => format!(
                "ChatGPT plan account {name:?} signed out locally; remote revocation not \
                 confirmed — finish in ChatGPT settings."
            ),
            Self::AlreadySignedOut { name } => {
                format!("ChatGPT plan account {name:?} is already signed out.")
            }
        }
    }
}

pub async fn run(name: &str) -> anyhow::Result<()> {
    let outcome = run_with(name, &Endpoints::production(), &Behavior::production()).await?;
    println!("{}", outcome.render());
    Ok(())
}

pub async fn run_with(
    name: &str,
    endpoints: &Endpoints,
    behavior: &Behavior,
) -> anyhow::Result<Outcome> {
    shared::validate_account_name(name)?;
    let record = read(name).await?.with_context(|| {
        format!("no chatgpt-plan account named {name:?}; run `shunt login chatgpt-plan --name {name}` first")
    })?;
    let Some(tokens) = record.tokens.as_ref() else {
        return Ok(Outcome::AlreadySignedOut { name: name.into() });
    };
    let confirmed = revoke(
        endpoints,
        behavior,
        &record.client_id,
        &tokens.refresh_token,
    )
    .await;
    let unchanged = clear(name, &tokens.refresh_token).await?;
    let name = name.to_string();
    Ok(if confirmed && unchanged {
        Outcome::Confirmed { name }
    } else {
        Outcome::NotConfirmed { name }
    })
}

/// Whether the issuer confirmed the revocation. Every failure is `false`; no
/// error text is kept because none of it is needed and none may carry a token.
async fn revoke(
    endpoints: &Endpoints,
    behavior: &Behavior,
    client_id: &str,
    refresh_token: &str,
) -> bool {
    if refresh_token.is_empty() {
        return false;
    }
    let Some(endpoint) = discover(endpoints, behavior).await else {
        return false;
    };
    let response = with_retries(behavior, || {
        token_refresh_client()
            .post(endpoint.clone())
            .form(&[
                ("token", refresh_token),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id),
            ])
            .send()
    })
    .await;
    // RFC 7009: 200 even when the token was already invalid.
    response.is_some_and(|response| response.status() == StatusCode::OK)
}

/// The issuer's revocation endpoint, only when it is https (or loopback http
/// under test) and on the issuer's own host.
async fn discover(endpoints: &Endpoints, behavior: &Behavior) -> Option<Url> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        endpoints.issuer.trim_end_matches('/')
    );
    let response = with_retries(behavior, || token_refresh_client().get(&url).send()).await?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = tokio::time::timeout(behavior.attempt_timeout, response.json())
        .await
        .ok()?
        .ok()?;
    let endpoint = body.get("revocation_endpoint")?.as_str()?;
    accept_endpoint(endpoint, &endpoints.issuer, behavior.allow_http)
}

fn accept_endpoint(endpoint: &str, issuer: &str, allow_http: bool) -> Option<Url> {
    let endpoint: Url = endpoint.parse().ok()?;
    let issuer: Url = issuer.parse().ok()?;
    let scheme_ok = endpoint.scheme() == "https" || (allow_http && endpoint.scheme() == "http");
    let same_host = endpoint.host_str().is_some() && endpoint.host_str() == issuer.host_str();
    (scheme_ok && same_host && endpoint.username().is_empty() && endpoint.password().is_none())
        .then_some(endpoint)
}

/// Send until a non-5xx response arrives or the attempts run out, sleeping a
/// doubling backoff between tries. A network error, a timeout and a 5xx are retried; any
/// other status is returned to the caller as final.
async fn with_retries<F, Fut>(behavior: &Behavior, mut send: F) -> Option<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    let mut delay = behavior.backoff;
    for attempt in 1..=behavior.attempts.max(1) {
        match tokio::time::timeout(behavior.attempt_timeout, send()).await {
            Ok(Ok(response)) if !response.status().is_server_error() => return Some(response),
            _ if attempt == behavior.attempts.max(1) => return None,
            _ => {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
        }
    }
    None
}

async fn read(name: &str) -> anyhow::Result<Option<AccountRecord>> {
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let _guard = lock_account(&name)?;
        store::read_account(&name)
    })
    .await
    .context("account read task failed")?
}

/// Rewrite the account as `{version, client_id}` under the account lock. The
/// client id is re-read under the lock so a concurrent re-registration is kept.
///
/// Returns whether the stored refresh token is still `revoked`. The lock is not
/// held across the revocation, so a gateway refresh or a new sign-in can
/// replace the token meanwhile; the replacement is cleared locally but was
/// never revoked, and the caller must not report the sign-out as confirmed.
async fn clear(name: &str, revoked: &str) -> anyhow::Result<bool> {
    let name = name.to_string();
    let revoked = revoked.to_string();
    tokio::task::spawn_blocking(move || {
        let guard = lock_account(&name)?;
        let current = store::read_account(&name)?.with_context(|| {
            format!("chatgpt-plan account {name:?} disappeared during sign-out")
        })?;
        let unchanged = current
            .tokens
            .as_ref()
            .is_some_and(|tokens| tokens.refresh_token == revoked);
        store::write_account_locked(&guard, &name, &current.into_signed_out())?;
        Ok(unchanged)
    })
    .await
    .context("account write task failed")?
}

fn lock_account(name: &str) -> anyhow::Result<file_lock::FileLock> {
    file_lock::lock_file_blocking(
        &store::account_path(name),
        &ACCOUNT_FILE_LOCK,
        CLI_LOCK_TIMEOUT,
    )
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)] // Intentional cross-module test serialization.
mod tests;
