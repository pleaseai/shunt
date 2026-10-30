//! Refresh against a loopback token endpoint and a temp accounts dir.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::auth::chatgpt_plan::record::RECORD_VERSION;
use crate::auth::shared::EnvVarGuard;

const NAME: &str = "primary";
const CLIENT_ID: &str = "oaiapp_issued";

struct Env {
    // Drop order: the env var is restored before the lock is released.
    _env: EnvVarGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: PathBuf,
    server: MockServer,
}

impl Env {
    async fn new(tag: &str) -> Self {
        let lock = crate::config::test_env_lock();
        let dir = std::env::temp_dir().join(format!(
            "shunt-plan-auth-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let env = EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
        Self {
            _env: env,
            _lock: lock,
            dir,
            server: MockServer::start().await,
        }
    }

    fn store(&self) -> ChatGptPlanAuthStore {
        ChatGptPlanAuthStore::with_token_url(format!("{}/oauth/token", self.server.uri()))
    }

    async fn posts(&self) -> usize {
        self.server.received_requests().await.unwrap().len()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn account(expires_in: i64, earliest_in: Option<i64>) -> AccountRecord {
    let at = |delta: i64| (unix_now() as i64 + delta) as u64;
    AccountRecord {
        version: RECORD_VERSION,
        client_id: CLIENT_ID.into(),
        sub: Some("user-1".into()),
        email: Some("a@example.com".into()),
        scopes: vec!["openid".into()],
        tokens: Some(TokenBlock {
            access_token: "access-old".into(),
            refresh_token: "refresh-old".into(),
            id_token: Some("id-old".into()),
            expires_at: at(expires_in),
            earliest_refresh_at: earliest_in.map(at),
            last_refresh: Some(1),
        }),
    }
}

fn ok_body() -> serde_json::Value {
    json!({
        "access_token": "access-new",
        "refresh_token": "refresh-new",
        "expires_in": 3600,
        "scope": "openid",
        "earliest_refresh_at": unix_now() + 3000,
    })
}

async fn mount(env: &Env, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .respond_with(response)
        .mount(&env.server)
        .await;
}

fn stored() -> AccountRecord {
    store::read_account(NAME).unwrap().unwrap()
}

fn assert_no_secrets(error: &ChatGptAuthError) {
    let rendered = format!("{error:?} {} {}", error.detail, error.error.message);
    for secret in [
        "access-old",
        "refresh-old",
        "id-old",
        "access-new",
        "refresh-new",
    ] {
        assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
    }
}

#[tokio::test]
async fn token_in_refresh_window_is_renewed_and_rotation_persisted() {
    let env = Env::new("renew").await;
    store::write_account(NAME, &account(100, None)).unwrap();
    mount(&env, ResponseTemplate::new(200).set_body_json(ok_body())).await;

    let token = env.store().get_valid(NAME).await.unwrap();
    assert_eq!(token, "access-new");

    let requests = env.server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let form = String::from_utf8(requests[0].body.clone()).unwrap();
    for part in [
        "grant_type=refresh_token",
        "refresh_token=refresh-old",
        "client_id=oaiapp_issued",
        "resource=https%3A%2F%2Fapi.openai.com%2Fv1",
    ] {
        assert!(form.contains(part), "missing {part} in {form}");
    }

    let tokens = stored().tokens.unwrap();
    assert_eq!(tokens.access_token, "access-new");
    assert_eq!(tokens.refresh_token, "refresh-new");
    assert_eq!(tokens.id_token.as_deref(), Some("id-old"));
    assert!(tokens.expires_at.abs_diff(unix_now() + 3600) <= 5);
    assert!(
        tokens
            .earliest_refresh_at
            .unwrap()
            .abs_diff(unix_now() + 3000)
            <= 5
    );
    assert!(tokens.last_refresh.unwrap().abs_diff(unix_now()) <= 5);
    assert_eq!(stored().client_id, CLIENT_ID);
    // The renewed token is outside its window now: no second POST.
    assert_eq!(env.store().get_valid(NAME).await.unwrap(), "access-new");
    assert_eq!(env.posts().await, 1);
}

#[tokio::test]
async fn token_outside_the_window_is_returned_without_a_post() {
    let env = Env::new("fresh").await;
    store::write_account(NAME, &account(3600, None)).unwrap();
    mount(&env, ResponseTemplate::new(500)).await;

    assert_eq!(env.store().get_valid(NAME).await.unwrap(), "access-old");
    assert_eq!(env.posts().await, 0);
}

#[tokio::test]
async fn no_post_before_earliest_refresh_at_unless_expired() {
    let env = Env::new("earliest").await;
    mount(&env, ResponseTemplate::new(200).set_body_json(ok_body())).await;

    // In the window, not yet expired, server says wait: keep the old token.
    store::write_account(NAME, &account(100, Some(1000))).unwrap();
    assert_eq!(env.store().get_valid(NAME).await.unwrap(), "access-old");
    assert_eq!(env.posts().await, 0);

    // Already expired: the floor no longer applies.
    store::write_account(NAME, &account(-10, Some(1000))).unwrap();
    assert_eq!(env.store().get_valid(NAME).await.unwrap(), "access-new");
    assert_eq!(env.posts().await, 1);
}

#[tokio::test]
async fn concurrent_callers_trigger_one_post() {
    let env = Env::new("concurrent").await;
    store::write_account(NAME, &account(100, None)).unwrap();
    mount(
        &env,
        ResponseTemplate::new(200)
            .set_body_json(ok_body())
            .set_delay(Duration::from_millis(200)),
    )
    .await;

    let store = env.store();
    let results = futures_util::future::join_all((0..6).map(|_| store.get_valid(NAME))).await;
    for result in results {
        assert_eq!(result.unwrap(), "access-new");
    }
    assert_eq!(env.posts().await, 1);
    assert_eq!(stored().tokens.unwrap().refresh_token, "refresh-new");
}

#[tokio::test]
async fn cancelled_caller_does_not_lose_the_rotated_token() {
    let env = Env::new("cancel").await;
    store::write_account(NAME, &account(100, None)).unwrap();
    mount(
        &env,
        ResponseTemplate::new(200)
            .set_body_json(ok_body())
            .set_delay(Duration::from_millis(300)),
    )
    .await;

    let store = env.store();
    let caller = tokio::spawn({
        let store = store.clone();
        async move { store.get_valid(NAME).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());

    // The detached task finishes the writeback; the next caller reuses it.
    assert_eq!(store.get_valid(NAME).await.unwrap(), "access-new");
    assert_eq!(env.posts().await, 1);
    assert_eq!(stored().tokens.unwrap().refresh_token, "refresh-new");
}

#[tokio::test]
async fn terminal_grant_errors_are_terminal_and_leave_the_file_alone() {
    for code in ["invalid_grant", "invalid_client"] {
        let env = Env::new(code).await;
        let before = account(100, None);
        store::write_account(NAME, &before).unwrap();
        mount(
            &env,
            ResponseTemplate::new(400).set_body_json(json!({"error": code})),
        )
        .await;

        let error = env.store().get_valid(NAME).await.unwrap_err();
        assert_eq!(
            error.terminal,
            Some(TerminalRefresh::InvalidGrant),
            "{code}"
        );
        assert!(error.detail.contains(code));
        assert_no_secrets(&error);
        assert_eq!(stored(), before);
    }
}

#[tokio::test]
async fn server_errors_rate_limits_and_timeouts_are_transient() {
    for status in [500, 503, 429] {
        let env = Env::new("transient").await;
        store::write_account(NAME, &account(100, None)).unwrap();
        mount(
            &env,
            ResponseTemplate::new(status).set_body_string("access-old refresh-old"),
        )
        .await;
        let error = env.store().get_valid(NAME).await.unwrap_err();
        assert_eq!(error.terminal, None, "{status}");
        assert_no_secrets(&error);
    }

    let env = Env::new("timeout").await;
    store::write_account(NAME, &account(100, None)).unwrap();
    mount(
        &env,
        ResponseTemplate::new(200)
            .set_body_json(ok_body())
            .set_delay(Duration::from_secs(5)),
    )
    .await;
    let error = env
        .store()
        .with_refresh_timeout(Duration::from_millis(150))
        .get_valid(NAME)
        .await
        .unwrap_err();
    assert_eq!(error.terminal, None);

    let dead = ChatGptPlanAuthStore::with_token_url("http://127.0.0.1:1/oauth/token".into());
    let error = dead.get_valid(NAME).await.unwrap_err();
    assert_eq!(error.terminal, None);
    assert_no_secrets(&error);
}

#[tokio::test]
async fn missing_signed_out_and_corrupt_accounts_are_terminal() {
    let env = Env::new("gone").await;
    mount(&env, ResponseTemplate::new(200).set_body_json(ok_body())).await;

    let missing = env.store().get_valid(NAME).await.unwrap_err();
    assert_eq!(missing.terminal, Some(TerminalRefresh::NoRefreshToken));

    store::write_signed_out(NAME, CLIENT_ID).unwrap();
    let signed_out = env.store().get_valid(NAME).await.unwrap_err();
    assert_eq!(signed_out.terminal, Some(TerminalRefresh::NoRefreshToken));

    std::fs::write(store::account_path(NAME), "{not json").unwrap();
    let corrupt = env.store().get_valid(NAME).await.unwrap_err();
    assert_eq!(corrupt.terminal, Some(TerminalRefresh::NoRefreshToken));

    let mut empty = account(100, None);
    empty.tokens.as_mut().unwrap().refresh_token.clear();
    store::write_account(NAME, &empty).unwrap();
    let no_refresh = env.store().get_valid(NAME).await.unwrap_err();
    assert_eq!(no_refresh.terminal, Some(TerminalRefresh::NoRefreshToken));
    assert_eq!(env.posts().await, 0);
}

#[tokio::test]
async fn signed_out_while_the_post_is_in_flight_discards_the_pair() {
    let env = Env::new("logout").await;
    store::write_account(NAME, &account(100, None)).unwrap();
    // The logout lands after the refresh read the file but before its write.
    let path = store::account_path(NAME);
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            std::fs::write(
                &path,
                format!(r#"{{"version":1,"client_id":"{CLIENT_ID}"}}"#),
            )
            .unwrap();
            ResponseTemplate::new(200).set_body_json(ok_body())
        })
        .mount(&env.server)
        .await;

    let error = env.store().get_valid(NAME).await.unwrap_err();
    assert_eq!(error.terminal, Some(TerminalRefresh::NoRefreshToken));
    assert_no_secrets(&error);
    assert_eq!(stored(), AccountRecord::signed_out(CLIENT_ID));
    let raw = std::fs::read_to_string(store::account_path(NAME)).unwrap();
    assert!(!raw.contains("access-new") && !raw.contains("refresh-new"));
}

#[tokio::test]
async fn response_without_refresh_token_keeps_the_old_one() {
    let env = Env::new("norotate").await;
    store::write_account(NAME, &account(100, None)).unwrap();
    mount(
        &env,
        ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-new", "expires_in": 600,
        })),
    )
    .await;

    assert_eq!(env.store().get_valid(NAME).await.unwrap(), "access-new");
    let tokens = stored().tokens.unwrap();
    assert_eq!(tokens.refresh_token, "refresh-old");
    assert_eq!(tokens.access_token, "access-new");
    assert_eq!(tokens.earliest_refresh_at, None);
}

#[tokio::test]
async fn force_refresh_only_replaces_the_rejected_token() {
    let env = Env::new("force").await;
    store::write_account(NAME, &account(3000, None)).unwrap();
    mount(&env, ResponseTemplate::new(200).set_body_json(ok_body())).await;
    let store_ = env.store();

    // Disk already holds a different token than the one upstream rejected.
    let token = store_
        .force_refresh_if_access_token(NAME, "some-other-token")
        .await
        .unwrap();
    assert_eq!(token, "access-old");
    assert_eq!(env.posts().await, 0);

    // The rejected token is still current: renewed even though unexpired.
    let token = store_
        .force_refresh_if_access_token(NAME, "access-old")
        .await
        .unwrap();
    assert_eq!(token, "access-new");
    assert_eq!(env.posts().await, 1);

    // A second 401 report for the same (now replaced) token does not re-POST.
    let token = store_
        .force_refresh_if_access_token(NAME, "access-old")
        .await
        .unwrap();
    assert_eq!(token, "access-new");
    assert_eq!(env.posts().await, 1);
}

#[tokio::test]
async fn account_file_lock_is_held_across_the_post() {
    let env = Env::new("filelock").await;
    store::write_account(NAME, &account(100, None)).unwrap();
    let blocked = Arc::new(AtomicBool::new(false));
    let seen = blocked.clone();
    let path = store::account_path(NAME);
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            let attempt = file_lock::lock_file_blocking(
                &path,
                &ACCOUNT_FILE_LOCK,
                Duration::from_millis(200),
            );
            seen.store(attempt.is_err(), Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(ok_body())
        })
        .mount(&env.server)
        .await;

    env.store().get_valid(NAME).await.unwrap();
    assert!(
        blocked.load(Ordering::SeqCst),
        "lock was free during the POST"
    );
    // Released afterwards.
    file_lock::lock_file_blocking(
        &store::account_path(NAME),
        &ACCOUNT_FILE_LOCK,
        Duration::from_millis(500),
    )
    .unwrap();
}

#[test]
fn refresh_rule_boundaries() {
    let tokens = |expires_at, earliest| TokenBlock {
        access_token: "a".into(),
        refresh_token: "r".into(),
        id_token: None,
        expires_at,
        earliest_refresh_at: earliest,
        last_refresh: None,
    };
    assert!(!refresh_needed(&tokens(1301, None), None, 1000));
    assert!(refresh_needed(&tokens(1300, None), None, 1000));
    assert!(!refresh_needed(&tokens(1300, Some(1001)), None, 1000));
    assert!(refresh_needed(&tokens(1300, Some(1000)), None, 1000));
    assert!(refresh_needed(&tokens(1000, Some(5000)), None, 1000));
}
