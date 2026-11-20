//! Claude Desktop support: the client-id-less device flow and
//! `GET /user/bootstrap`.

use super::*;

const DEVICE_GRANT_FORM: &str = "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code";

fn bootstrap_request(bearer: Option<&str>, if_none_match: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri("/user/bootstrap");
    if let Some(bearer) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    if let Some(value) = if_none_match {
        builder = builder.header(header::IF_NONE_MATCH, value);
    }
    builder.body(Body::empty()).unwrap()
}

fn write_template(label: &str, template: &Value) -> std::path::PathBuf {
    let (dir, _, _) = temp_config_path(label);
    let path = dir.join("bootstrap.json");
    std::fs::write(&path, serde_json::to_vec_pretty(template).unwrap()).unwrap();
    path
}

fn set_template(config: &mut Config, path: &std::path::Path) {
    config.server.gateway.as_mut().unwrap().desktop_bootstrap =
        Some(crate::config::GatewayDesktopBootstrapConfig {
            template: path.to_path_buf(),
        });
}

fn template() -> Value {
    json!({
        "$schemaVersion": "bootstrap-config-v2",
        "otlp": {
            "endpoint": "https://otel.example",
            "resourceAttributes": {"user.email": "${user.email}", "team": "core"}
        },
        "flat_user": "${user.name} <${user.sub}>",
        "${user.email}": "keys are not substituted",
        "unknown_key": {"keep": [1, true, null, "${other}"]},
        "unknown_token": "x-${other}-y"
    })
}

/// Raw response parts: status, headers, body bytes.
async fn raw(
    router: &Router,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, body.to_vec())
}

fn assert_no_store(headers: &axum::http::HeaderMap) {
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
}

#[tokio::test]
async fn desktop_device_flow_without_client_id_signs_in_refreshes_and_revokes_with_400() {
    let (mut config, mut env) = GatewayEnv::config("desktop-flow");
    let auth_env = format!("SHUNT_GATEWAY_TEST_DESKTOP_CLIENT_{}", std::process::id());
    env.set_var(auth_env.clone(), "static:static-token");
    config.server.auth = Some(InboundAuthConfig {
        jwt: Vec::new(),
        header: "x-shunt-token".into(),
        tokens_env: auth_env.clone(),
        ..Default::default()
    });
    let template_path = write_template("desktop-flow", &template());
    set_template(&mut config, &template_path);
    let (router, _, _) = build_router(config).unwrap();

    // Desktop posts an empty form: no client_id, no scope.
    let (status, authorization) = json_response(
        router.clone(),
        form_request("/oauth/device_authorization", ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let device_code = authorization["device_code"].as_str().unwrap();
    let user_code = authorization["user_code"].as_str().unwrap();
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/device")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::ORIGIN, "https://gateway.example")
                .body(Body::from(format!(
                    "user_code={user_code}&login=dev%40example.com&secret=password"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let (status, token) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!("{DEVICE_GRANT_FORM}&device_code={device_code}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let access = token["access_token"].as_str().unwrap().to_string();
    let refresh = token["refresh_token"].as_str().unwrap().to_string();

    // The same bearer is accepted for inference and for the bootstrap config;
    // `/v1/models` is gated, so a missing bearer is the control.
    let models = |bearer: Option<&str>| {
        let mut builder = Request::builder().uri("/v1/models");
        if let Some(bearer) = bearer {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        builder.body(Body::empty()).unwrap()
    };
    let (status, _) = json_response(router.clone(), models(None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = json_response(router.clone(), models(Some(&access))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = raw(&router, bootstrap_request(Some(&access), None)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, refreshed) = json_response(
        router.clone(),
        form_request(
            "/oauth/token",
            format!("grant_type=refresh_token&refresh_token={refresh}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(refreshed["refresh_token"], refresh.as_str());

    // Desktop only recognises a revocation as 400 + invalid_grant.
    let (status, error) = json_response(
        router,
        form_request(
            "/oauth/token",
            format!("grant_type=refresh_token&refresh_token={refresh}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error, json!({"error": "invalid_grant"}));
}

#[tokio::test]
async fn bootstrap_is_404_without_a_template_and_401_without_a_valid_bearer() {
    let (config, env) = GatewayEnv::config("bootstrap-unconfigured");
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("dev@example.com");
    let (status, headers, body) = raw(&router, bootstrap_request(Some(&bearer), None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_no_store(&headers);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"]["type"],
        "not_found_error"
    );
    drop(env);

    let (mut config, _env) = GatewayEnv::config("bootstrap-401");
    let path = write_template("bootstrap-401", &template());
    set_template(&mut config, &path);
    let (router, _, _) = build_router(config).unwrap();
    for request in [
        bootstrap_request(None, None),
        bootstrap_request(Some("garbage"), None),
    ] {
        let (status, headers, body) = raw(&router, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_no_store(&headers);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"]["type"],
            "authentication_error"
        );
    }
}

#[tokio::test]
async fn bootstrap_substitutes_user_values_only_and_keeps_the_rest_byte_equal() {
    let (mut config, _env) = GatewayEnv::config("bootstrap-substitute");
    let path = write_template("bootstrap-substitute", &template());
    set_template(&mut config, &path);
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("dev@example.com");

    let (status, headers, body) = raw(&router, bootstrap_request(Some(&bearer), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_no_store(&headers);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let etag = headers
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(etag.starts_with("\"sha256:") && etag.ends_with('"'));
    assert_eq!(etag, format!("\"sha256:{:x}\"", Sha256::digest(&body)));

    let served: Value = serde_json::from_slice(&body).unwrap();
    let mut expected = template();
    expected["otlp"]["resourceAttributes"]["user.email"] = json!("dev@example.com");
    expected["flat_user"] = json!("dev <dev@example.com>");
    assert_eq!(served, expected);

    let (status, headers, body) = raw(&router, bootstrap_request(Some(&bearer), Some(&etag))).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers.get(header::ETAG).unwrap().to_str().unwrap(), etag);
    assert_no_store(&headers);
    assert!(body.is_empty());

    // The template is re-read per request: no router rebuild needed.
    let mut changed = template();
    changed["otlp"]["endpoint"] = json!("https://other.example");
    std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    let (status, headers, body) = raw(&router, bootstrap_request(Some(&bearer), Some(&etag))).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(headers.get(header::ETAG).unwrap().to_str().unwrap(), etag);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["otlp"]["endpoint"],
        "https://other.example"
    );
}

#[tokio::test]
async fn bootstrap_template_that_goes_bad_after_boot_is_a_generic_500() {
    let (mut config, _env) = GatewayEnv::config("bootstrap-500");
    let path = write_template("bootstrap-500", &template());
    set_template(&mut config, &path);
    let (router, _, _) = build_router(config).unwrap();
    let bearer = gateway_bearer("dev@example.com");

    for contents in ["not json", "[1, 2]"] {
        std::fs::write(&path, contents).unwrap();
        let (status, headers, body) = raw(&router, bootstrap_request(Some(&bearer), None)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_no_store(&headers);
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("api_error") && !text.contains(path.to_str().unwrap()));
    }
    std::fs::remove_file(&path).unwrap();
    let (status, headers, _) = raw(&router, bootstrap_request(Some(&bearer), None)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_no_store(&headers);
}

#[tokio::test]
async fn bootstrap_is_403_for_users_no_managed_policy_covers() {
    let (mut config, _env) = GatewayEnv::config("bootstrap-403");
    let path = write_template("bootstrap-403", &template());
    set_template(&mut config, &path);
    config.server.gateway.as_mut().unwrap().policies = Some(vec![policy(
        Some(vec!["alice@example.com"]),
        toml::toml! { marker = "alice" },
    )]);
    let (router, _, _) = build_router(config.clone()).unwrap();
    let alice = gateway_bearer("alice@example.com");
    let bob = gateway_bearer("bob@example.com");

    let (status, _, _) = raw(&router, bootstrap_request(Some(&alice), None)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, body) = raw(&router, bootstrap_request(Some(&bob), None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_no_store(&headers);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"]["type"],
        "permission_error"
    );

    config.server.gateway.as_mut().unwrap().policies = Some(vec![
        policy(
            Some(vec!["alice@example.com"]),
            toml::toml! { marker = "a" },
        ),
        policy(None, toml::toml! { shared = true }),
    ]);
    let (router, _, _) = build_router(config).unwrap();
    let (status, _, _) = raw(&router, bootstrap_request(Some(&bob), None)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn policy_emails_match_the_token_email_case_insensitively() {
    let (mut config, _env) = GatewayEnv::config("bootstrap-mixed-case");
    let path = write_template("bootstrap-mixed-case", &template());
    set_template(&mut config, &path);
    config.server.gateway.as_mut().unwrap().policies = Some(vec![policy(
        Some(vec!["Alice@Example.com"]),
        toml::toml! { marker = "alice" },
    )]);
    let (router, _, _) = build_router(config).unwrap();

    // Policy written in mixed case, IdP reporting yet another casing.
    let alice = gateway_bearer("ALICE@example.COM");
    let (status, _, _) = raw(&router, bootstrap_request(Some(&alice), None)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = json_response(router.clone(), managed_request(Some(&alice), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["settings"]["marker"], "alice");

    let bob = gateway_bearer("Bob@Example.com");
    let (status, _, _) = raw(&router, bootstrap_request(Some(&bob), None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[test]
fn config_rejects_a_missing_or_non_object_bootstrap_template() {
    let (mut config, _env) = GatewayEnv::config("bootstrap-config");
    let missing = std::env::temp_dir().join(format!("shunt-missing-{}.json", std::process::id()));
    set_template(&mut config, &missing);
    let gateway = config.server.gateway.clone().unwrap();
    assert!(matches!(
        gateway.resolve(),
        Err(crate::config::ConfigError::UnreadableDesktopBootstrap { .. })
    ));

    for contents in ["[1, 2]", "not json"] {
        let (dir, _, _) = temp_config_path("bootstrap-config-bad");
        let path = dir.join("bootstrap.json");
        std::fs::write(&path, contents).unwrap();
        set_template(&mut config, &path);
        let gateway = config.server.gateway.clone().unwrap();
        assert!(matches!(
            gateway.resolve(),
            Err(crate::config::ConfigError::InvalidDesktopBootstrap { .. })
        ));
    }
}
