//! Token refresh for chatgpt-plan accounts.
//!
//! Mirrors the xAI store, not the Codex one (which needs a `chatgpt_account_id`
//! claim these tokens lack). The pool asks for a usable access token per
//! request ([`ChatGptPlanAuthStore::get_valid`]) and, after an upstream 401,
//! for a replacement only if the rejected token is still the one on disk
//! ([`ChatGptPlanAuthStore::force_refresh_if_access_token`]).
//!
//! The refresh token rotates on every use, so a refresh must never run twice
//! with one token and its result must never be lost:
//!
//! - an in-process single-flight lock per account file serializes callers;
//! - the POST and the writeback run in a detached task that owns that lock, so
//!   a cancelled caller cannot drop a rotated token;
//! - the cross-process account file lock is held across read, POST and write,
//!   because `shunt login` / `shunt logout` can run beside the gateway;
//! - a file that was signed out meanwhile is never resurrected.
//!
//! Token values never reach an error string or a log line (NFR-2).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use super::{
    authorize::RESOURCE,
    exchange::TokenResponse,
    record::{AccountRecord, TokenBlock},
    store::{self, ACCOUNT_FILE_LOCK, ACCOUNT_LOCK_TIMEOUT},
};
use crate::auth::{
    codex::auth::{log_excerpt, ChatGptAuthError, TerminalRefresh},
    shared::{
        self,
        file_lock::{self, FileLock},
        refresh_lock::{normalized_auth_path, refresh_lock_for},
    },
};

const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";

/// Test seam for the gateway's refresh endpoint; see
/// [`ChatGptPlanAuthStore::from_env`].
const TOKEN_URL_ENV: &str = "SHUNT_CHATGPT_PLAN_TOKEN_URL";

/// The refresh endpoint for a raw `SHUNT_CHATGPT_PLAN_TOKEN_URL` value:
/// [`TOKEN_URL`] unless the value is loopback or https on its host.
fn resolve_token_url(raw: Option<String>) -> String {
    shared::sanitize_token_url(raw, TOKEN_URL)
}

/// An access token is renewed once it is this close to expiry.
const REFRESH_WINDOW_SECS: u64 = 300;

/// Upper bound on one caller's wait for the in-process lock and the refresh,
/// and, separately, on the token POST itself. A healthy refresh is one small
/// form POST, so this only trips on a stalled endpoint.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

const RELOGIN: &str = "run `shunt login chatgpt-plan --name <account-name>` again";

#[derive(Debug, Clone)]
pub struct ChatGptPlanAuthStore {
    token_url: String,
    refresh_timeout: Duration,
}

impl Default for ChatGptPlanAuthStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatGptPlanAuthStore {
    pub fn new() -> Self {
        Self::with_token_url(TOKEN_URL.to_string())
    }

    /// The gateway's store: [`TOKEN_URL`], unless `SHUNT_CHATGPT_PLAN_TOKEN_URL`
    /// names a loopback URL or https on the same host. That override is a test
    /// seam (as `SHUNT_CODEX_TOKEN_URL` is for codex), and
    /// [`shared::sanitize_token_url`] rejects any other value, so it cannot
    /// redirect the rotating refresh token off-origin or into the clear.
    pub fn from_env() -> Self {
        Self::with_token_url(resolve_token_url(std::env::var(TOKEN_URL_ENV).ok()))
    }

    /// A store that refreshes against `token_url` (tests inject a loopback
    /// mock). Callers outside tests go through [`Self::new`] or
    /// [`Self::from_env`], never an arbitrary URL: a redirectable token URL
    /// would be a refresh-token exfiltration surface.
    pub fn with_token_url(token_url: String) -> Self {
        Self {
            token_url,
            refresh_timeout: REFRESH_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_refresh_timeout(mut self, refresh_timeout: Duration) -> Self {
        self.refresh_timeout = refresh_timeout;
        self
    }

    /// The account's current access token, renewed first when it is inside the
    /// refresh window.
    pub async fn get_valid(&self, name: &str) -> Result<String, ChatGptAuthError> {
        self.ensure_fresh(name, None).await
    }

    /// Upstream rejected `stale_token`: renew only if it is still the access
    /// token on disk. Another caller (or process) may already have replaced
    /// it, in which case that replacement is returned without a POST. A forced
    /// refresh ignores `earliest_refresh_at` while the stored token is still
    /// the rejected one.
    pub async fn force_refresh_if_access_token(
        &self,
        name: &str,
        stale_token: &str,
    ) -> Result<String, ChatGptAuthError> {
        self.ensure_fresh(name, Some(stale_token)).await
    }

    async fn ensure_fresh(
        &self,
        name: &str,
        stale: Option<&str>,
    ) -> Result<String, ChatGptAuthError> {
        shared::validate_account_name(name)
            .map_err(|_| ChatGptAuthError::new("invalid account name"))?;
        let (path, tokens) = read_tokens_off_thread(name, true).await?;
        if !refresh_needed(&tokens, stale, unix_now()) {
            return Ok(tokens.access_token);
        }

        // One deadline bounds this caller's wait: for the lock, then for the
        // refresh task. The token POST itself is not cut short by it.
        let deadline = Instant::now() + self.refresh_timeout;
        let refreshing = tokio::time::timeout_at(deadline, refresh_lock_for(&path).lock_owned())
            .await
            .map_err(|_| {
                ChatGptAuthError::new(
                    "ChatGPT plan token refresh timed out waiting on a concurrent refresh",
                )
            })?;
        // A caller that held the lock before us has refreshed: use its token.
        let (_, tokens) = read_tokens_off_thread(name, false).await?;
        if !refresh_needed(&tokens, stale, unix_now()) {
            return Ok(tokens.access_token);
        }

        // Detached so a dropped caller cannot release the lock mid-writeback
        // and expose the consumed refresh token to the next one.
        let name = name.to_string();
        let stale = stale.map(ToOwned::to_owned);
        let token_url = self.token_url.clone();
        let refresh_timeout = self.refresh_timeout;
        let handle = tokio::spawn(async move {
            let _refreshing = refreshing; // held until the writeback completes
            refresh_under_file_lock(&name, stale.as_deref(), &token_url, refresh_timeout).await
        });
        tokio::time::timeout_at(deadline, handle)
            .await
            .map_err(|_| {
                ChatGptAuthError::new("ChatGPT plan token refresh timed out; the token endpoint did not answer in time")
            })?
            .map_err(|error| {
                ChatGptAuthError::new(format!("ChatGPT plan refresh task failed: {error}"))
            })?
    }
}

/// Whether the stored token must be renewed now.
///
/// Routine (`stale` is `None`): due inside the window, never before
/// `earliest_refresh_at` unless the token has already expired. Forced (`stale`
/// set, upstream rejected that token): due exactly when the stored token is
/// still the rejected one, whatever the window or the floor, since the same
/// token would only be rejected again.
fn refresh_needed(tokens: &TokenBlock, stale: Option<&str>, now: u64) -> bool {
    if let Some(stale) = stale {
        return tokens.access_token == stale;
    }
    let expired = now >= tokens.expires_at;
    let in_window = now >= tokens.expires_at.saturating_sub(REFRESH_WINDOW_SECS);
    let too_early = tokens.earliest_refresh_at.is_some_and(|at| now < at);
    in_window && (expired || !too_early)
}

/// Read the account's tokens on the blocking pool. A missing, corrupt or
/// signed-out file is terminal; an I/O failure (permissions) is transient.
/// `resolve` also returns the resolved path the in-process lock is keyed by.
async fn read_tokens_off_thread(
    name: &str,
    resolve: bool,
) -> Result<(std::path::PathBuf, TokenBlock), ChatGptAuthError> {
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let path = if resolve {
            normalized_auth_path(store::account_path(&name))
        } else {
            std::path::PathBuf::new()
        };
        signed_in_tokens(&name).map(|(_, tokens)| (path, tokens))
    })
    .await
    .map_err(|error| ChatGptAuthError::new(format!("account read task failed: {error}")))?
}

/// The account record and its tokens, or the error the pool should see.
fn signed_in_tokens(name: &str) -> Result<(AccountRecord, TokenBlock), ChatGptAuthError> {
    match store::read_account(name) {
        Ok(Some(record)) => match record.tokens.clone() {
            Some(tokens) if !tokens.access_token.is_empty() && !tokens.refresh_token.is_empty() => {
                Ok((record, tokens))
            }
            _ => Err(no_refresh_token(format!(
                "ChatGPT plan account {name} is signed out; {RELOGIN}"
            ))),
        },
        Ok(None) => Err(no_refresh_token(format!(
            "ChatGPT plan account {name} not found; {RELOGIN}"
        ))),
        Err(error) => {
            // A plain I/O failure may clear on its own; a file that does not
            // parse never will.
            let io_failure = error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .is_some();
            let detail = format!("ChatGPT plan account {name} unreadable: {error:#}");
            if io_failure {
                Err(ChatGptAuthError::new(detail))
            } else {
                Err(no_refresh_token(format!("{detail}; {RELOGIN}")))
            }
        }
    }
}

fn no_refresh_token(detail: String) -> ChatGptAuthError {
    terminal(TerminalRefresh::NoRefreshToken, detail)
}

fn terminal(kind: TerminalRefresh, detail: String) -> ChatGptAuthError {
    ChatGptAuthError {
        terminal: Some(kind),
        ..ChatGptAuthError::new(detail)
    }
}

/// Read, POST and write with the account file lock held throughout.
async fn refresh_under_file_lock(
    name: &str,
    stale: Option<&str>,
    token_url: &str,
    refresh_timeout: Duration,
) -> Result<String, ChatGptAuthError> {
    // Another gateway sharing the account may hold the lock across its own
    // POST, so wait out that hold rather than cooling a usable account. The
    // caller's deadline still bounds the caller.
    let guard = file_lock::lock_file(
        &store::account_path(name),
        &ACCOUNT_FILE_LOCK,
        refresh_timeout + ACCOUNT_LOCK_TIMEOUT,
    )
    .await
    .map_err(|error| ChatGptAuthError::new(format!("{error:#}")))?;

    // Another process may have refreshed (or signed out) before we got the lock.
    let name_owned = name.to_string();
    let (record, tokens) = tokio::task::spawn_blocking(move || signed_in_tokens(&name_owned))
        .await
        .map_err(|error| ChatGptAuthError::new(format!("account read task failed: {error}")))??;
    if !refresh_needed(&tokens, stale, unix_now()) {
        return Ok(tokens.access_token);
    }

    let response = tokio::time::timeout(
        refresh_timeout,
        post_refresh(token_url, &record.client_id, &tokens.refresh_token),
    )
    .await
    .map_err(|_| {
        ChatGptAuthError::new(
            "ChatGPT plan token refresh timed out; the token endpoint did not answer",
        )
    })??;

    let rotated = response
        .refresh_token
        .as_deref()
        .is_some_and(|new| !new.is_empty() && new != tokens.refresh_token);
    let name_owned = name.to_string();
    let (_guard, committed) = tokio::task::spawn_blocking(move || {
        let committed = commit_refresh(&guard, &name_owned, response, unix_now());
        (guard, committed)
    })
    .await
    .map_err(|error| ChatGptAuthError::new(format!("account write task failed: {error}")))?;
    match committed {
        Ok(access_token) => {
            tracing::info!(account = name, "refreshed ChatGPT plan access token");
            Ok(access_token)
        }
        Err(CommitError::SignedOut(error)) => Err(error),
        Err(CommitError::Write(error)) => {
            let detail = format!("failed to persist the refreshed ChatGPT plan tokens: {error:#}");
            Err(if rotated {
                terminal(TerminalRefresh::WritebackFailed, detail)
            } else {
                ChatGptAuthError::new(detail)
            })
        }
    }
}

enum CommitError {
    /// The account was signed out (or removed) while the POST was in flight.
    SignedOut(ChatGptAuthError),
    Write(anyhow::Error),
}

/// Merge the refreshed pair into the file as it is *now* and write it, under
/// the caller's lock. A file that lost its tokens meanwhile is left alone: the
/// refreshed pair is discarded rather than resurrecting a signed-out account.
fn commit_refresh(
    lock: &FileLock,
    name: &str,
    response: TokenResponse,
    now: u64,
) -> Result<String, CommitError> {
    let (mut record, mut tokens) = signed_in_tokens(name).map_err(CommitError::SignedOut)?;
    match response.refresh_token.filter(|token| !token.is_empty()) {
        Some(refresh_token) => tokens.refresh_token = refresh_token,
        None => tracing::warn!(
            account = name,
            "ChatGPT plan refresh response omitted refresh_token; keeping the stored one"
        ),
    }
    if let Some(id_token) = response.id_token.filter(|token| !token.is_empty()) {
        tokens.id_token = Some(id_token);
    }
    tokens.access_token = response.access_token.clone();
    tokens.expires_at = now.saturating_add(response.expires_in.unwrap_or(3600));
    tokens.earliest_refresh_at = response.earliest_refresh_at;
    tokens.last_refresh = Some(now);
    record.tokens = Some(tokens);
    store::write_account_locked(lock, name, &record).map_err(CommitError::Write)?;
    Ok(response.access_token)
}

/// The refresh POST, through the redirect-hardened client. Errors carry the
/// status and the OAuth error code only, never request or body text.
async fn post_refresh(
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse, ChatGptAuthError> {
    let response = shared::token_refresh_client()
        .post(token_url)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("resource", RESOURCE),
        ])
        .send()
        .await
        .map_err(|error| {
            ChatGptAuthError::new(format!(
                "failed to refresh ChatGPT plan auth: {}",
                error.without_url()
            ))
        })?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|_| ChatGptAuthError::new("invalid ChatGPT plan refresh response"))?;
    if !status.is_success() {
        let code = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("error")?.as_str().map(log_excerpt));
        return Err(match code.as_deref() {
            Some(code @ ("invalid_grant" | "invalid_client")) => terminal(
                TerminalRefresh::InvalidGrant,
                format!("ChatGPT rejected the stored credentials ({code}); {RELOGIN}"),
            ),
            Some(code) => ChatGptAuthError::new(format!(
                "failed to refresh ChatGPT plan auth ({status}): oauth error `{code}`"
            )),
            None => {
                ChatGptAuthError::new(format!("failed to refresh ChatGPT plan auth ({status})"))
            }
        });
    }
    serde_json::from_str::<TokenResponse>(&text)
        .ok()
        .filter(|response| !response.access_token.is_empty())
        .ok_or_else(|| ChatGptAuthError::new("invalid ChatGPT plan refresh response"))
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
