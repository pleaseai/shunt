use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    response::Response,
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::{
    config::{Config, GatewayAdminConfig, GatewayConfig, GatewayEnforcementConfig, GroupLimitMode},
    server::build_router,
};

const WRITE_KEY: &str = "write-key-0123456789abcdef0123456789";
const READ_KEY: &str = "read-key-0123456789abcdef01234567890";
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct SpendEnv {
    _guard: MutexGuard<'static, ()>,
    secret_env: String,
    users_env: String,
    write_env: String,
    read_env: String,
}

impl SpendEnv {
    fn config(label: &str) -> (Config, Self) {
        Self::config_with_state_path(label, None)
    }

    fn config_with_state_path(label: &str, state_path: Option<PathBuf>) -> (Config, Self) {
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let suffix = format!("{}_{}", std::process::id(), label);
        let secret_env = format!("SHUNT_SPEND_TEST_SECRET_{suffix}");
        let users_env = format!("SHUNT_SPEND_TEST_USERS_{suffix}");
        let write_env = format!("SHUNT_SPEND_TEST_WRITE_{suffix}");
        let read_env = format!("SHUNT_SPEND_TEST_READ_{suffix}");
        std::env::set_var(&secret_env, "0123456789abcdef0123456789abcdef");
        std::env::set_var(&users_env, "dev@example.com:password");
        std::env::set_var(&write_env, format!("writer:{WRITE_KEY}"));
        std::env::set_var(&read_env, format!("reader:{READ_KEY}"));
        let mut config = Config::default();
        config.server.gateway = Some(GatewayConfig {
            public_url: "https://gateway.example".into(),
            jwt_secret_env: secret_env.clone(),
            users_env: users_env.clone(),
            token_ttl_seconds: 3600,
            trust_forwarded_for: false,
            policies: None,
            telemetry: None,
            state_path: None,
            admin: Some(GatewayAdminConfig {
                write_keys_env: write_env.clone(),
                read_keys_env: read_env.clone(),
                blocked_message: None,
                audit_retention_days: 365,
                spend_retention_months: 13,
                identity_retention_days: 90,
                group_limit_mode: GroupLimitMode::Min,
                state_path,
                write_keys: Vec::new(),
                read_keys: Vec::new(),
            }),
            enforcement: GatewayEnforcementConfig::default(),
            oidc: None,
        });
        (
            config,
            Self {
                _guard: guard,
                secret_env,
                users_env,
                write_env,
                read_env,
            },
        )
    }
}

impl Drop for SpendEnv {
    fn drop(&mut self) {
        for key in [
            &self.secret_env,
            &self.users_env,
            &self.write_env,
            &self.read_env,
        ] {
            std::env::remove_var(key);
        }
    }
}

async fn send(router: &Router, request: Request<Body>) -> (Response, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (Response::from_parts(parts, Body::empty()), value)
}

fn request(method: &str, path: &str, key: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("x-api-key", key);
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&body).unwrap())
        }
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn post(router: &Router, key: &str, body: Value) -> (Response, Value) {
    send(
        router,
        request("POST", "/v1/organizations/spend_limits", key, Some(body)),
    )
    .await
}

fn assert_error_response(response: &Response, body: &Value, status: StatusCode) {
    assert_eq!(response.status(), status);
    assert_eq!(body["type"], "error");
    assert_eq!(
        response.headers()["request-id"].to_str().unwrap(),
        body["request_id"].as_str().unwrap()
    );
}

async fn raw_post(router: &Router, body: &'static str) -> (Response, Value) {
    send(
        router,
        Request::post("/v1/organizations/spend_limits")
            .header("x-api-key", WRITE_KEY)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

fn organization(amount: Value, period: &str) -> Value {
    json!({"scope":{"type":"organization"},"amount":amount,"period":period})
}

fn user(user_id: &str, amount: Value, period: &str) -> Value {
    json!({"scope":{"type":"user","user_id":user_id},"amount":amount,"period":period})
}

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "shunt-spend-api-{}-{}-{label}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).expect("create temp directory");
    path
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn failing_state_path(directory: &Path) -> PathBuf {
    let blocker = directory.join("blocker");
    std::fs::write(&blocker, b"file blocks directory creation").expect("create blocker file");
    blocker.join("state.json")
}

#[tokio::test]
async fn upsert_preserves_identity_and_distinguishes_unlimited_from_zero() {
    let (config, _env) = SpendEnv::config("upsert");
    let (router, _, _) = build_router(config).unwrap();
    let (_, first) = post(&router, WRITE_KEY, organization(Value::Null, "monthly")).await;
    assert_eq!(first["amount"], Value::Null);
    let (_, second) = post(&router, WRITE_KEY, organization(json!("0"), "monthly")).await;
    assert_eq!(second["amount"], "0");
    assert_eq!(second["id"], first["id"]);
    assert_eq!(second["created_at"], first["created_at"]);
    assert!(second["updated_at"].as_str().unwrap() >= first["updated_at"].as_str().unwrap());
}

#[tokio::test]
async fn invalid_amount_currency_and_unsupported_scope_return_400() {
    let (config, _env) = SpendEnv::config("validation");
    let (router, _, _) = build_router(config).unwrap();
    for amount in [json!("1.5"), json!("-1"), json!("nope")] {
        let (response, _) = post(&router, WRITE_KEY, organization(amount, "monthly")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let (response, _) = post(
        &router,
        WRITE_KEY,
        json!({"scope":{"type":"organization"},"amount":"1","currency":"EUR"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let (response, body) = post(
        &router,
        WRITE_KEY,
        json!({"scope":{"type":"rbac_group","rbac_group_id":"eng"},"amount":"1"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("does not support scope type"));
}

#[tokio::test]
async fn amount_and_user_id_length_boundaries_are_enforced() {
    let (config, _env) = SpendEnv::config("length-bounds");
    let (router, _, _) = build_router(config).unwrap();
    let max_amount = "9".repeat(super::store::MAX_AMOUNT_LENGTH);
    let max_user_id = "u".repeat(super::store::MAX_USER_ID_LENGTH);
    let (response, _) = post(
        &router,
        WRITE_KEY,
        user(&max_user_id, json!(max_amount), "monthly"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let (response, body) = post(
        &router,
        WRITE_KEY,
        organization(
            json!("9".repeat(super::store::MAX_AMOUNT_LENGTH + 1)),
            "monthly",
        ),
    )
    .await;
    assert_error_response(&response, &body, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains(&super::store::MAX_AMOUNT_LENGTH.to_string()));

    let (response, body) = post(
        &router,
        WRITE_KEY,
        user(
            &"u".repeat(super::store::MAX_USER_ID_LENGTH + 1),
            json!("1"),
            "monthly",
        ),
    )
    .await;
    assert_error_response(&response, &body, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains(&super::store::MAX_USER_ID_LENGTH.to_string()));
}

#[tokio::test]
async fn read_key_is_get_only_bad_key_is_unauthorized_and_request_ids_match_errors() {
    let (config, _env) = SpendEnv::config("auth");
    let (router, _, _) = build_router(config).unwrap();
    let (created_response, created) =
        post(&router, WRITE_KEY, organization(json!("1"), "daily")).await;
    assert!(created_response.headers().contains_key("request-id"));
    let id = created["id"].as_str().unwrap();
    let (get_response, _) = send(
        &router,
        request(
            "GET",
            &format!("/v1/organizations/spend_limits/{id}"),
            READ_KEY,
            None,
        ),
    )
    .await;
    assert_eq!(get_response.status(), StatusCode::OK);
    let (post_response, _) = post(&router, READ_KEY, organization(json!("2"), "daily")).await;
    assert_eq!(post_response.status(), StatusCode::FORBIDDEN);
    let (delete_response, _) = send(
        &router,
        request(
            "DELETE",
            &format!("/v1/organizations/spend_limits/{id}"),
            READ_KEY,
            None,
        ),
    )
    .await;
    assert_eq!(delete_response.status(), StatusCode::FORBIDDEN);
    let (bad_response, bad_body) = send(
        &router,
        request("GET", "/v1/organizations/spend_limits", "bad", None),
    )
    .await;
    assert_eq!(bad_response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        bad_response.headers()["request-id"].to_str().unwrap(),
        bad_body["request_id"].as_str().unwrap()
    );
}

#[tokio::test]
async fn list_paginates_forward_and_backward_with_directional_has_more() {
    let (config, _env) = SpendEnv::config("pagination");
    let (router, _, _) = build_router(config).unwrap();
    let mut ids = Vec::new();
    for period in ["daily", "weekly", "monthly"] {
        let (_, body) = post(&router, WRITE_KEY, organization(json!("1"), period)).await;
        ids.push(body["id"].as_str().unwrap().to_string());
    }
    let (_, first) = send(
        &router,
        request(
            "GET",
            "/v1/organizations/spend_limits?limit=1",
            READ_KEY,
            None,
        ),
    )
    .await;
    assert_eq!(first["data"][0]["id"], ids[0]);
    assert_eq!(first["has_more"], true);
    let (_, after) = send(
        &router,
        request(
            "GET",
            &format!("/v1/organizations/spend_limits?limit=2&after_id={}", ids[0]),
            READ_KEY,
            None,
        ),
    )
    .await;
    assert_eq!(after["last_id"], ids[2]);
    assert_eq!(after["has_more"], false);
    let (_, before) = send(
        &router,
        request(
            "GET",
            &format!(
                "/v1/organizations/spend_limits?limit=1&before_id={}",
                ids[2]
            ),
            READ_KEY,
            None,
        ),
    )
    .await;
    assert_eq!(before["data"][0]["id"], ids[1]);
    assert_eq!(before["has_more"], true);
    let (max_response, max_page) = send(
        &router,
        request(
            "GET",
            "/v1/organizations/spend_limits?limit=1000",
            READ_KEY,
            None,
        ),
    )
    .await;
    assert_eq!(max_response.status(), StatusCode::OK);
    assert_eq!(max_page["data"].as_array().unwrap().len(), 3);
    for path in [
        format!(
            "/v1/organizations/spend_limits?after_id={}&before_id={}",
            ids[0], ids[2]
        ),
        "/v1/organizations/spend_limits?limit=0".to_string(),
        "/v1/organizations/spend_limits?limit=1001".to_string(),
    ] {
        let (response, _) = send(&router, request("GET", &path, READ_KEY, None)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn list_rejects_unknown_cursors() {
    let (config, _env) = SpendEnv::config("unknown-cursors");
    let (router, _, _) = build_router(config).unwrap();
    post(&router, WRITE_KEY, organization(json!("1"), "daily")).await;
    for cursor in ["after_id", "before_id"] {
        let path = format!("/v1/organizations/spend_limits?{cursor}=spl_does_not_exist");
        let (response, body) = send(&router, request("GET", &path, READ_KEY, None)).await;
        assert_error_response(&response, &body, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&format!("unknown {cursor}")));
    }
}

#[tokio::test]
async fn list_filters_supported_scope_types_and_rejects_others() {
    let (config, _env) = SpendEnv::config("scope-filter");
    let (router, _, _) = build_router(config).unwrap();
    let (_, organization_limit) = post(&router, WRITE_KEY, organization(json!("1"), "daily")).await;
    let (_, user_limit) = post(&router, WRITE_KEY, user("usr_filter", json!("2"), "weekly")).await;

    for (scope_type, included, excluded) in [
        ("user", &user_limit, &organization_limit),
        ("organization", &organization_limit, &user_limit),
    ] {
        let path = format!("/v1/organizations/spend_limits?scope_type={scope_type}");
        let (response, body) = send(&router, request("GET", &path, READ_KEY, None)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body["data"].as_array().unwrap().len(), 1);
        assert_eq!(body["data"][0]["id"], included["id"]);
        assert_ne!(body["data"][0]["id"], excluded["id"]);
    }

    for (scope_type, expected) in [
        ("rbac_group", "does not support scope type"),
        ("seat_tier", "does not support scope type"),
        ("organization_service", "does not support scope type"),
        ("unknown", "invalid scope_type"),
    ] {
        let path = format!("/v1/organizations/spend_limits?scope_type={scope_type}");
        let (response, body) = send(&router, request("GET", &path, READ_KEY, None)).await;
        assert_error_response(&response, &body, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains(expected));
    }
}

#[tokio::test]
async fn extractor_failures_use_the_admin_error_envelope() {
    let (config, _env) = SpendEnv::config("rejections");
    let (router, _, _) = build_router(config).unwrap();
    for body in [
        r#"{"scope":{"type":"organization"}}"#,
        r#"{"scope":{"type":"organization"},"amount":1}"#,
        r#"{"scope":{"type":"organization"},"amount":"1","period":"yearly"}"#,
        r#"{"scope":{"type":"organization"},"amount":"1""#,
    ] {
        let (response, value) = raw_post(&router, body).await;
        assert_error_response(&response, &value, StatusCode::BAD_REQUEST);
    }
    let (response, value) = send(
        &router,
        request(
            "GET",
            "/v1/organizations/spend_limits?limit=not-a-number",
            READ_KEY,
            None,
        ),
    )
    .await;
    assert_error_response(&response, &value, StatusCode::BAD_REQUEST);

    let (response, value) = send(
        &router,
        request("PATCH", "/v1/organizations/spend_limits", WRITE_KEY, None),
    )
    .await;
    assert_error_response(&response, &value, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn create_persistence_failure_rolls_back_store() {
    let directory = TempDir(temp_dir("create-persist-failure"));
    let state_path = failing_state_path(&directory.0);
    let (config, _env) =
        SpendEnv::config_with_state_path("create-persist-failure", Some(state_path));
    let (router, _, state) = build_router(config).unwrap();

    let (response, body) = post(&router, WRITE_KEY, organization(json!("100"), "monthly")).await;
    assert_error_response(&response, &body, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["type"], "api_error");
    assert!(state.gateway_stores.spend.list().is_empty());
    assert!(state.gateway_stores.spend.export().audit.is_empty());
}

#[tokio::test]
async fn delete_persistence_failure_rolls_back_store() {
    let directory = TempDir(temp_dir("delete-persist-failure"));
    let state_path = failing_state_path(&directory.0);
    let (config, _env) =
        SpendEnv::config_with_state_path("delete-persist-failure", Some(state_path));
    let (router, _, state) = build_router(config).unwrap();
    let created = state.gateway_stores.spend.upsert(
        super::store::Scope::Organization,
        super::store::Period::Monthly,
        Some("100".into()),
        "admin-key:writer",
        "2026-08-10T00:00:00.000Z".into(),
    );
    let id = created.id.clone();

    let (response, body) = send(
        &router,
        request(
            "DELETE",
            &format!("/v1/organizations/spend_limits/{id}"),
            WRITE_KEY,
            None,
        ),
    )
    .await;
    assert_error_response(&response, &body, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["type"], "api_error");
    assert_eq!(state.gateway_stores.spend.get(&id), Some(created));
    assert_eq!(state.gateway_stores.spend.export().audit.len(), 1);
}

#[tokio::test]
async fn delete_and_unknown_ids_use_the_pinned_shapes() {
    let (config, _env) = SpendEnv::config("delete");
    let (router, _, _) = build_router(config).unwrap();
    let (_, created) = post(&router, WRITE_KEY, organization(json!("1"), "daily")).await;
    let id = created["id"].as_str().unwrap();
    let (response, deleted) = send(
        &router,
        request(
            "DELETE",
            &format!("/v1/organizations/spend_limits/{id}"),
            WRITE_KEY,
            None,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(deleted, json!({"id": id, "type": "spend_limit_deleted"}));

    let (response, body) = send(
        &router,
        request(
            "GET",
            "/v1/organizations/spend_limits/spl_missing",
            READ_KEY,
            None,
        ),
    )
    .await;
    assert_error_response(&response, &body, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn routes_are_absent_without_gateway_admin_config() {
    let response = build_router(Config::default())
        .unwrap()
        .0
        .oneshot(
            Request::get("/v1/organizations/spend_limits")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Routes are registered from the presence of `[server.gateway.admin]` alone,
/// so a block whose key env vars resolve to nothing leaves the surface present
/// but unusable. The companion test in `config.rs` only proves the boot warning
/// fires and the lists come back empty; it would not catch an `authenticate`
/// refactor that read "no keys configured" as "auth disabled". Assert the
/// fail-closed behavior over HTTP, where that regression would actually show.
#[tokio::test]
async fn configured_admin_block_without_resolved_keys_denies_every_request() {
    let (config, env) = SpendEnv::config("keyless");
    // Keep the admin block, drop only the key material the harness exported.
    std::env::set_var(&env.write_env, "");
    std::env::set_var(&env.read_env, "");

    let (router, _, _) = build_router(config).unwrap();

    // The routes still exist — this is not the absent-config 404 case.
    let (response, body) = send(
        &router,
        request("GET", "/v1/organizations/spend_limits", WRITE_KEY, None),
    )
    .await;
    assert_error_response(&response, &body, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "authentication_error");

    let (response, body) = post(&router, WRITE_KEY, organization(json!("100"), "monthly")).await;
    assert_error_response(&response, &body, StatusCode::UNAUTHORIZED);

    let (response, body) = send(
        &router,
        request("GET", "/v1/organizations/spend_limits", READ_KEY, None),
    )
    .await;
    assert_error_response(&response, &body, StatusCode::UNAUTHORIZED);
}
