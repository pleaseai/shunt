//! `shunt login chatgpt-plan --name <account>`: Sign in with ChatGPT.
//!
//! One attempt: fresh PKCE, state and nonce (memory only) -> authorize URL ->
//! the loopback listener races a pasted callback URL -> code exchange -> ID
//! token validation -> persist. Nothing is written before the ID token
//! validates, and nothing secret is ever printed.

use std::{
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context};
use tokio::sync::mpsc;

use super::{
    authorize::{self, AuthorizeRequest, PLAN_USAGE_SCOPE},
    exchange::{self, TokenResponse},
    id_token::{self, IdTokenError, OPENAI_ISSUER},
    paste::{self, PasteReceiver},
    record::{AccountRecord, TokenBlock, RECORD_VERSION},
    store::{self, ACCOUNT_FILE_LOCK, ACCOUNT_LOCK_TIMEOUT},
};
use crate::auth::{
    callback::{CallbackConfig, CallbackParams, CallbackServer},
    inbound_jwt::JwksCache,
    shared::{self, file_lock, generate_pkce, random_urlsafe, PkceChallenge},
};

const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
/// The redirect URI OpenAI's dynamic registration accepts for local agents.
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Where the sign-in talks. Production uses the constants; tests inject
/// loopback mocks. There is deliberately no environment override: a redirectable
/// issuer or token URL would be a credential-exfiltration surface.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize_url: String,
    pub token_url: String,
    pub issuer: String,
    /// `0` picks an ephemeral port (tests).
    pub callback_port: u16,
}

impl Endpoints {
    pub fn production() -> Self {
        Self {
            authorize_url: AUTHORIZE_URL.into(),
            token_url: TOKEN_URL.into(),
            issuer: OPENAI_ISSUER.into(),
            callback_port: CALLBACK_PORT,
        }
    }
}

/// Knobs that tests turn; production uses [`Behavior::production`].
pub struct Behavior {
    pub timeout: Duration,
    pub open_browser: bool,
    /// Receives the authorize URL instead of stdout when set.
    pub url_sink: Option<mpsc::UnboundedSender<String>>,
}

impl Behavior {
    pub fn production() -> Self {
        Self {
            timeout: SIGN_IN_TIMEOUT,
            open_browser: true,
            url_sink: None,
        }
    }
}

/// The non-secret result of a sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub name: String,
    pub email: Option<String>,
    pub scopes: Vec<String>,
}

impl Outcome {
    pub fn render(&self) -> String {
        let who = self
            .email
            .as_deref()
            .unwrap_or("(no email on the ID token)");
        format!(
            "ChatGPT plan account {:?} signed in as {who}.\nScopes granted: {}",
            self.name,
            self.scopes.join(" ")
        )
    }
}

pub async fn run(name: &str) -> anyhow::Result<()> {
    let outcome = run_with(
        name,
        &Endpoints::production(),
        Behavior::production(),
        paste::spawn_stdin_reader,
    )
    .await?;
    println!("{}", outcome.render());
    Ok(())
}

pub async fn run_with(
    name: &str,
    endpoints: &Endpoints,
    behavior: Behavior,
    start_paste: impl FnOnce() -> PasteReceiver,
) -> anyhow::Result<Outcome> {
    shared::validate_account_name(name)?;
    let saved = saved_client_id(name).await?;
    let host_id = tokio::task::spawn_blocking(store::host_id)
        .await
        .context("host id task failed")??;
    let PkceChallenge {
        verifier,
        challenge,
        state,
    } = generate_pkce();
    let nonce = random_urlsafe(32);

    let config = CallbackConfig {
        label: "ChatGPT",
        port: endpoints.callback_port,
        path: CALLBACK_PATH,
        host: "127.0.0.1",
    };
    let (listener, redirect_uri) = match CallbackServer::bind(config, state.clone()).await {
        Ok(server) => {
            let uri = server.redirect_uri();
            (Some(server), uri)
        }
        Err(error) => {
            eprintln!(
                "Could not listen for the browser callback ({error:#}). Finish the sign-in by \
                 pasting the final URL from the browser's address bar."
            );
            let uri = format!(
                "http://127.0.0.1:{}{CALLBACK_PATH}",
                endpoints.callback_port
            );
            (None, uri)
        }
    };
    let url = authorize::build_url(&AuthorizeRequest {
        authorize_url: &endpoints.authorize_url,
        saved_client_id: saved.as_deref(),
        host_id: &host_id,
        redirect_uri: &redirect_uri,
        state: &state,
        nonce: &nonce,
        challenge: &challenge,
    })?;
    match &behavior.url_sink {
        Some(sink) => {
            let _ = sink.send(url.to_string());
        }
        None => {
            println!("Open this URL to sign in with ChatGPT:\n\n    {url}\n");
        }
    }
    if behavior.open_browser {
        if let Err(error) = shared::open_url_async("ChatGPT sign-in", url.as_str()).await {
            eprintln!("Could not open the browser automatically: {error}");
        }
    }

    let received = await_callback(listener, start_paste(), &state, behavior.timeout).await?;
    let client_id = authorize::bind_client_id(saved.as_deref(), received.client_id.as_deref())?;
    let tokens = exchange::exchange_code(
        &endpoints.token_url,
        &client_id,
        &received.code,
        &verifier,
        &redirect_uri,
    )
    .await?;
    let identity = verify(endpoints, &client_id, &nonce, &tokens).await?;

    let scopes: Vec<String> = tokens
        .scope
        .as_deref()
        .or(received.scope.as_deref())
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if !scopes.iter().any(|scope| scope == PLAN_USAGE_SCOPE) {
        // Keep the registration so the next sign-in reuses the client id.
        commit(name, saved, AccountRecord::signed_out(&client_id)).await?;
        bail!(
            "plan usage was not granted ({PLAN_USAGE_SCOPE} is missing from the granted scopes); \
             the account stays signed out. Approve plan usage and run the sign-in again"
        );
    }

    let now = unix_now();
    let record = AccountRecord {
        version: RECORD_VERSION,
        client_id,
        sub: Some(identity.sub),
        email: identity.email.clone(),
        scopes: scopes.clone(),
        tokens: Some(TokenBlock {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token.unwrap_or_default(),
            id_token: tokens.id_token,
            expires_at: now.saturating_add(tokens.expires_in.unwrap_or(3600)),
            earliest_refresh_at: tokens.earliest_refresh_at,
            last_refresh: Some(now),
        }),
    };
    commit(name, saved, record).await?;
    Ok(Outcome {
        name: name.to_string(),
        email: identity.email,
        scopes,
    })
}

/// Race the listener against the pasted URL; the first usable one wins. A
/// paste that cannot be read (EOF, no terminal) only ends the paste side.
async fn await_callback(
    listener: Option<CallbackServer>,
    paste: PasteReceiver,
    state: &str,
    timeout: Duration,
) -> anyhow::Result<CallbackParams> {
    let has_listener = listener.is_some();
    let mut listening: Pin<Box<dyn Future<Output = anyhow::Result<CallbackParams>> + Send>> =
        match listener {
            Some(server) => Box::pin(server.wait_for_callback(timeout)),
            None => Box::pin(std::future::pending()),
        };
    let mut paste = Some(paste);
    let race = async {
        loop {
            tokio::select! {
                result = &mut listening => return result,
                pasted = async { paste.as_mut().expect("guarded").await }, if paste.is_some() => {
                    paste = None;
                    match pasted {
                        Ok(Ok(text)) if !text.trim().is_empty() => {
                            return authorize::parse_pasted(&text, state);
                        }
                        _ if has_listener => {}
                        Ok(Err(error)) => return Err(error.context("could not read the pasted URL")),
                        _ => bail!("no callback URL was entered"),
                    }
                }
            }
        }
    };
    tokio::time::timeout(timeout, race)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for the ChatGPT sign-in"))?
}

async fn verify(
    endpoints: &Endpoints,
    client_id: &str,
    nonce: &str,
    tokens: &TokenResponse,
) -> anyhow::Result<id_token::VerifiedIdentity> {
    let token = tokens
        .id_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .filter(|_| {
            tokens
                .refresh_token
                .as_deref()
                .is_some_and(|t| !t.is_empty())
        })
        .context("the sign-in failed validation: the token response was incomplete")?;
    id_token::verify_id_token(
        &JwksCache::new(),
        &endpoints.issuer,
        client_id,
        nonce,
        token,
    )
    .await
    .map_err(|error| match error {
        IdTokenError::Rejected => anyhow::anyhow!(
            "the ChatGPT sign-in failed validation (ID token rejected); nothing was stored"
        ),
        IdTokenError::KeysUnavailable => anyhow::anyhow!("{error}; nothing was stored"),
    })
}

/// The saved registration's client id, read under the account lock.
async fn saved_client_id(name: &str) -> anyhow::Result<Option<String>> {
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let _guard = lock_account(&name)?;
        Ok(store::read_account(&name)?.map(|record| record.client_id))
    })
    .await
    .context("account read task failed")?
}

/// Write `record` if the saved client id is still what the attempt started
/// from. The lock is held across the read and the write, so the write goes
/// through the lock-free atomic helper (the store's own writer takes the lock).
async fn commit(name: &str, expected: Option<String>, record: AccountRecord) -> anyhow::Result<()> {
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let _guard = lock_account(&name)?;
        let current = store::read_account(&name)?.map(|saved| saved.client_id);
        anyhow::ensure!(
            current == expected,
            "the account changed while you were signing in; nothing was stored. Run the sign-in again"
        );
        shared::write_auth_file_atomic(&store::account_path(&name), &serde_json::to_value(&record)?)?;
        Ok(())
    })
    .await
    .context("account write task failed")?
}

fn lock_account(name: &str) -> anyhow::Result<file_lock::FileLock> {
    file_lock::lock_file_blocking(
        &store::account_path(name),
        &ACCOUNT_FILE_LOCK,
        ACCOUNT_LOCK_TIMEOUT,
    )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)] // Intentional cross-module test serialization.
mod tests;
