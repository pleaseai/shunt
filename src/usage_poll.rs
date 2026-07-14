//! Background poller for the Anthropic OAuth usage API.
//!
//! When `[server.pool] usage_refresh_seconds` is set, this spawns one task at
//! boot that periodically polls `GET /api/oauth/usage` for every imported
//! (refreshable) Claude account across all `claude_oauth` providers, applying the
//! returned utilization to the account pool via [`AccountPool::note_usage`].
//!
//! Why: the pool's primary quota signal is the `anthropic-ratelimit-unified-*`
//! headers on proxied responses, which only reflect traffic that flowed through
//! shunt. When the same account is also used out-of-band (the operator's own
//! Claude Code, another tool), that consumption is invisible to the headers and
//! the pool undercounts. The usage API reports ground-truth utilization, so a
//! periodic poll reconciles the header-derived state.
//!
//! Eligibility: only imported logins can call the endpoint. A long-lived
//! `claude setup-token` (and an env-supplied `token_env` credential) is treated
//! as static and skipped, mirroring the adapter's non-refreshable 401 handling.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;

use crate::{
    accounts::AccountPool,
    auth::{self, claude, resolve_claude_account, Credential},
    config::{AccountConfig, AuthMode},
    server::AppState,
};

/// Spawn the usage poller if `[server.pool] usage_refresh_seconds` enables it.
/// A no-op otherwise, so the default deployment adds no background work. Whether
/// the task exists is decided once from the boot config (like the admin and
/// codex surfaces); a reload does not start or stop it.
pub fn spawn_usage_poller(state: AppState) {
    let Some(pool) = state.config.server.pool.as_ref() else {
        return;
    };
    let Some(interval) = pool.usage_refresh_interval() else {
        return;
    };
    // The interval floor is applied silently in config; surface the clamp so an
    // operator who set e.g. 30 isn't left wondering why the log below shows 60.
    if let Some(configured) = pool.usage_refresh_seconds {
        if configured != interval {
            tracing::warn!(
                configured_seconds = configured,
                effective_seconds = interval,
                "usage_refresh_seconds is below the 60s floor; using 60"
            );
        }
    }
    tracing::info!(
        interval_seconds = interval,
        "starting Claude OAuth usage-API poller"
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval));
        // A poll that runs long (or a suspend/resume) must not make the ticker
        // fire a burst of catch-up ticks — that would hammer the usage API. Skip
        // missed ticks and resume on the regular cadence.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // `interval` fires its first tick immediately, so pool state is seeded at
        // startup and then refreshed every `interval` seconds.
        loop {
            ticker.tick().await;
            poll_all(&state).await;
        }
    });
}

/// Poll every imported account of every `claude_oauth` provider once. Re-snapshots
/// the live shared state so a reloaded provider list / account set is picked up.
async fn poll_all(state: &AppState) {
    let state = state.refreshed();
    for (name, provider) in &state.config.providers {
        if provider.auth != AuthMode::ClaudeOauth {
            continue;
        }
        let accounts = match auth::shared::resolve_pool_accounts(
            "Claude",
            &provider.accounts,
            claude::store::default_accounts_dir(),
            claude::store::scan_accounts,
        )
        .await
        {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::debug!(provider = %name, %error, "usage poller: failed to resolve accounts");
                continue;
            }
        };
        for account in &accounts {
            poll_account(
                &state.http_client,
                &state.accounts,
                name,
                &provider.base_url,
                account,
            )
            .await;
        }
    }
}

/// Poll one account: skip non-refreshable credentials, resolve a valid access
/// token, fetch its usage, and apply it to the pool. Every failure degrades
/// quietly to a debug log — a missing snapshot just leaves the header-derived
/// state in place until the next tick.
async fn poll_account(
    client: &reqwest::Client,
    pool: &AccountPool,
    provider: &str,
    base_url: &str,
    account: &AccountConfig,
) {
    if !account_is_refreshable(account).await {
        return;
    }
    let credential = match resolve_claude_account(account, client).await {
        Ok(credential) => credential,
        Err(error) => {
            tracing::debug!(provider, account = %account.name, error = %error.message, "usage poller: failed to resolve account credential");
            return;
        }
    };
    let Credential::ClaudeOauth { access_token, .. } = credential else {
        return;
    };
    match claude::usage::fetch_usage(client, base_url, &access_token).await {
        Ok(snapshot) => {
            pool.note_usage(provider, &account.name, &snapshot);
            tracing::debug!(provider, account = %account.name, "usage poller: applied usage snapshot");
        }
        Err(error) => {
            tracing::debug!(provider, account = %account.name, %error, "usage poller: usage fetch failed");
        }
    }
}

/// Whether an account's credential is a refreshable imported login — the only
/// kind the usage API accepts. `token_env` credentials are treated as static.
/// The credential file (an explicit `credentials` path, or the store path for a
/// name-only account) is read on the blocking pool.
async fn account_is_refreshable(account: &AccountConfig) -> bool {
    if account.token_env.is_some() {
        return false;
    }
    let path = account
        .credentials
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| claude::store::account_path(&account.name));
    tokio::task::spawn_blocking(move || credential_file_has_refresh_token(&path))
        .await
        .unwrap_or(false)
}

/// True when the credential file holds a non-empty `claudeAiOauth.refreshToken`
/// — the signal the store uses to classify an imported login (vs a setup token).
fn credential_file_has_refresh_token(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| {
            value
                .pointer("/claudeAiOauth/refreshToken")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "shunt-usage-poll-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn account_with_credentials(path: &Path) -> AccountConfig {
        AccountConfig {
            name: "acct".to_string(),
            credentials: Some(path.to_string_lossy().into_owned()),
            token_env: None,
            uuid: None,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn refreshable_only_for_imported_credential_files() {
        // Imported login: has a non-empty refreshToken -> eligible.
        let imported = write_temp(
            "imported",
            r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        assert!(account_is_refreshable(&account_with_credentials(&imported)).await);

        // Setup token: no refreshToken -> not eligible.
        let setup = write_temp(
            "setup",
            r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":4000000000000,"shuntCredentialKind":"setup_token"}}"#,
        );
        assert!(!account_is_refreshable(&account_with_credentials(&setup)).await);

        // token_env credential is static regardless of any file.
        let env_account = AccountConfig {
            name: "env".to_string(),
            credentials: None,
            token_env: Some("SOME_ENV".to_string()),
            uuid: None,
            ..Default::default()
        };
        assert!(!account_is_refreshable(&env_account).await);

        // Missing file -> not eligible (no panic).
        let missing = AccountConfig {
            name: "nope".to_string(),
            credentials: Some("/no/such/shunt/usage/file.json".to_string()),
            token_env: None,
            uuid: None,
            ..Default::default()
        };
        assert!(!account_is_refreshable(&missing).await);

        for path in [imported, setup] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[tokio::test]
    async fn poll_account_fetches_and_applies_snapshot() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // An imported credential whose access token is far from expiry, so
        // resolve_claude_account returns it without hitting the token endpoint.
        let creds = write_temp(
            "poll",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        let account = account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": { "utilization": 20.0 },
                "seven_day": { "utilization": 75.0 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        poll_account(
            &reqwest::Client::new(),
            &pool,
            "anthropic",
            &server.uri(),
            &account,
        )
        .await;

        let snap = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert_eq!(snap.len(), 1);
        assert!(snap[0].has_state, "the poll must have recorded state");
        assert_eq!(snap[0].utilization_5h, Some(0.20));
        assert_eq!(snap[0].utilization_7d, Some(0.75));

        let _ = std::fs::remove_file(creds);
    }

    #[tokio::test]
    async fn poll_account_records_no_state_on_fetch_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A refreshable credential whose usage fetch fails (500): the poller must
        // degrade quietly, leaving the account with no recorded state.
        let creds = write_temp(
            "fetch-error",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        let account = account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        poll_account(
            &reqwest::Client::new(),
            &pool,
            "anthropic",
            &server.uri(),
            &account,
        )
        .await;

        let snap = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert!(
            !snap[0].has_state,
            "a failed usage fetch must not record state"
        );

        let _ = std::fs::remove_file(creds);
    }

    #[tokio::test]
    async fn poll_all_polls_only_claude_oauth_providers() {
        use crate::config::{ApiKeyHeader, Config, CountTokens, ProviderConfig, ProviderKind};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let creds = write_temp(
            "poll-all",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        let account = account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": { "utilization": 12.0 },
                "seven_day": { "utilization": 34.0 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Start from the default config (its `anthropic` provider is passthrough,
        // so `poll_all` must skip it) and add one `claude_oauth` provider pointed
        // at the mock usage server with an explicit imported account.
        let mut config = Config::default();
        config.providers.insert(
            "claude-pool".to_string(),
            ProviderConfig {
                kind: ProviderKind::Anthropic,
                base_url: server.uri(),
                auth: AuthMode::ClaudeOauth,
                api_key_env: None,
                api_key_header: ApiKeyHeader::Bearer,
                effort: None,
                count_tokens: CountTokens::default(),
                accounts: vec![account.clone()],
                websocket: false,
                tool_search: false,
                retry: Default::default(),
            },
        );
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        poll_all(&state).await;

        let snap =
            state
                .accounts
                .snapshot("claude-pool", std::slice::from_ref(&account), None, None);
        assert_eq!(snap.len(), 1);
        assert!(snap[0].has_state, "poll_all must apply the usage snapshot");
        assert_eq!(snap[0].utilization_5h, Some(0.12));
        assert_eq!(snap[0].utilization_7d, Some(0.34));

        let _ = std::fs::remove_file(creds);
    }

    #[tokio::test]
    async fn poll_account_skips_non_refreshable_without_fetching() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Setup-token file: the poller must not call the usage endpoint at all.
        let creds = write_temp(
            "skip",
            r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":4000000000000,"shuntCredentialKind":"setup_token"}}"#,
        );
        let account = account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        poll_account(
            &reqwest::Client::new(),
            &pool,
            "anthropic",
            &server.uri(),
            &account,
        )
        .await;

        let snap = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert!(!snap[0].has_state, "a skipped account records no state");

        let _ = std::fs::remove_file(creds);
    }

    #[tokio::test]
    async fn spawn_usage_poller_is_noop_without_pool_config() {
        // The default config has no `[server.pool] usage_refresh_seconds`, so the
        // spawn helper must take its guard path and start no background task.
        let state =
            AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();
        assert!(state.config.server.pool.is_none());
        spawn_usage_poller(state);
    }
}
