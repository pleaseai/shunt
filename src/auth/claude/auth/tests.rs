use super::*;

fn temp_credentials_path(tag: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "shunt-claude-{tag}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .join(".credentials.json")
}

/// Start a wiremock server that answers a single `POST /token` with a 200
/// carrying `new-access` and the given `refresh_token`. Shared by the token
/// tests whose mock setup is otherwise identical.
async fn mock_token_server(refresh_token: &str) -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access",
            "refresh_token": refresh_token,
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    server
}

fn write_credentials(path: &Path, access_token: &str, refresh_token: &str, expires_at: i64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "refreshToken": refresh_token,
                "expiresAt": expires_at
            }
        })
        .to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn cancelled_refresh_still_persists_rotated_token() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(json!({
                    "access_token": "new-access",
                    "refresh_token": "rotated-refresh",
                    "expires_in": 3600
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let path = temp_credentials_path("cancelled-refresh");
    write_credentials(&path, "expired-access", "old-refresh", 0);
    let store = ClaudeAuthStore::with_token_url(
        path.clone(),
        reqwest::Client::new(),
        format!("{}/token", server.uri()),
    );

    let caller = tokio::spawn(async move { store.get_valid_access_token().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let requests = server
                .received_requests()
                .await
                .expect("mock records requests");
            if requests
                .iter()
                .any(|request| request.method.as_str() == "POST" && request.url.path() == "/token")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("refresh request did not reach the OAuth provider");
    caller.abort();
    let error = caller.await.unwrap_err();
    assert!(error.is_cancelled());

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let stored = read_file(&path).unwrap();
            if stored["claudeAiOauth"]["refreshToken"] == "rotated-refresh" {
                assert_eq!(stored["claudeAiOauth"]["accessToken"], "new-access");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("detached refresh did not persist the rotated token");

    server.verify().await;
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn concurrent_get_valid_single_flights_refresh() {
    let server = mock_token_server("rotated-refresh").await;

    let path = temp_credentials_path("single-flight");
    write_credentials(&path, "expired-access", "old-refresh", 0);
    let first_store = ClaudeAuthStore::with_token_url(
        path.clone(),
        reqwest::Client::new(),
        format!("{}/token", server.uri()),
    );
    let second_store = ClaudeAuthStore::with_token_url(
        path.clone(),
        reqwest::Client::new(),
        format!("{}/token", server.uri()),
    );

    let (first, second) = tokio::join!(
        first_store.get_valid_access_token(),
        second_store.get_valid_access_token()
    );
    assert_eq!(first.unwrap(), "new-access");
    assert_eq!(second.unwrap(), "new-access");
    assert_eq!(
        read_file(&path).unwrap()["claudeAiOauth"]["refreshToken"],
        "rotated-refresh"
    );

    server.verify().await;
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn force_refresh_skips_when_rejected_token_was_already_replaced() {
    let path = temp_credentials_path("force-refresh-already-replaced");
    write_credentials(&path, "new-access", "rotated-refresh", 4_000_000_000_000);
    let store = ClaudeAuthStore::with_token_url(
        path.clone(),
        reqwest::Client::new(),
        "http://127.0.0.1:9/token".to_string(),
    );

    let token = store
        .force_refresh_if_access_token("rejected-access")
        .await
        .unwrap();

    assert_eq!(token, "new-access");
    let stored = read_file(&path).unwrap();
    assert_eq!(stored["claudeAiOauth"]["refreshToken"], "rotated-refresh");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn force_refresh_refreshes_a_still_valid_token() {
    let server = mock_token_server("new-refresh").await;

    let path = temp_credentials_path("force-refresh");
    write_credentials(&path, "still-valid", "old-refresh", 4_000_000_000_000);
    let store = ClaudeAuthStore::with_token_url(
        path.clone(),
        reqwest::Client::new(),
        format!("{}/token", server.uri()),
    );

    let token = store.force_refresh().await.unwrap();

    assert_eq!(token, "new-access");
    let stored = read_file(&path).unwrap();
    assert_eq!(stored["claudeAiOauth"]["accessToken"], "new-access");
    assert_eq!(stored["claudeAiOauth"]["refreshToken"], "new-refresh");
    server.verify().await;
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn valid_when_beyond_expiry_buffer() {
    let now = UNIX_EPOCH + Duration::from_secs(1_000);
    let inside = Tokens {
        access_token: "a".into(),
        refresh_token: None,
        expires_at_ms: (1_000 + 5 * 60 - 1) * 1000,
    };
    let outside = Tokens {
        access_token: "a".into(),
        refresh_token: None,
        expires_at_ms: (1_000 + 5 * 60 + 1) * 1000,
    };
    assert!(!inside.is_valid_at(now));
    assert!(outside.is_valid_at(now));
}

#[test]
fn sanitize_token_url_rejects_plaintext_off_origin_override() {
    // Parity with the Codex guard (#118): the Claude default is kept unless the
    // override is HTTPS or loopback HTTP, so a misconfigured env var can never
    // egress the long-lived refresh_token off-origin or in the clear. The Codex
    // store's own test exercises the full accept/reject matrix against the shared
    // guard; here we confirm the Claude wrapper binds it to the Claude default.
    assert_eq!(sanitize_token_url(None), TOKEN_URL);
    assert_eq!(
        sanitize_token_url(Some("http://malicious.test/oauth".to_string())),
        TOKEN_URL
    );
    assert_eq!(
        sanitize_token_url(Some("https://claude-mock.test/oauth".to_string())),
        "https://claude-mock.test/oauth"
    );
    assert_eq!(
        sanitize_token_url(Some("http://localhost:7000/oauth".to_string())),
        "http://localhost:7000/oauth"
    );
}

#[test]
fn parses_credentials_tokens() {
    let value = json!({
        "claudeAiOauth": {
            "accessToken": "sk-ant-oat-access",
            "refreshToken": "sk-ant-ort-refresh",
            "expiresAt": 2_000_000_000_000i64,
            "subscriptionType": "max"
        }
    });
    let tokens = Tokens::from_value(&value).unwrap();
    assert_eq!(tokens.access_token, "sk-ant-oat-access");
    assert_eq!(tokens.refresh_token.as_deref(), Some("sk-ant-ort-refresh"));
    assert_eq!(tokens.expires_at_ms, 2_000_000_000_000);
}

#[test]
fn refresh_reuses_prior_refresh_token_when_omitted() {
    let now = UNIX_EPOCH + Duration::from_secs(1_000);
    let value = json!({"access_token": "new-access", "expires_in": 3600});
    let refreshed = parse_refresh(&value, "old-refresh", now).unwrap();
    assert_eq!(refreshed.access_token, "new-access");
    assert_eq!(refreshed.refresh_token, "old-refresh");
    assert_eq!(refreshed.expires_at_ms, 1_000 * 1000 + 3600 * 1000);
}

#[test]
fn refresh_rejects_response_without_access_token() {
    let now = UNIX_EPOCH + Duration::from_secs(1_000);
    assert!(parse_refresh(&json!({"expires_in": 3600}), "old-refresh", now).is_none());
}

#[test]
fn write_back_updates_tokens_and_preserves_other_fields() {
    let path = temp_credentials_path("write-back");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"old-r","expiresAt":1,"subscriptionType":"max"},"mcpOAuth":{"keep":true}}"#,
    )
    .unwrap();

    write_back(
        &path,
        &Refreshed {
            access_token: "new".into(),
            refresh_token: "new-r".into(),
            expires_at_ms: 999,
        },
    )
    .unwrap();

    let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["claudeAiOauth"]["accessToken"], "new");
    assert_eq!(value["claudeAiOauth"]["refreshToken"], "new-r");
    assert_eq!(value["claudeAiOauth"]["expiresAt"], 999);
    assert_eq!(value["claudeAiOauth"]["subscriptionType"], "max");
    assert_eq!(value["mcpOAuth"]["keep"], true);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
