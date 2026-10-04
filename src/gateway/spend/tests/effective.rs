//! `GET /v1/organizations/spend_limits/effective`.

use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::{
    config::InboundAuthConfig,
    gateway::spend::{meter::window, store::Period},
    server::AppState,
};

const CENT: u64 = 10_000_000_000_000;
const PATH: &str = "/v1/organizations/spend_limits/effective";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn get(router: &Router, query: &str) -> (Response, Value) {
    get_as(router, query, READ_KEY).await
}

async fn get_as(router: &Router, query: &str, key: &str) -> (Response, Value) {
    send(router, request("GET", &format!("{PATH}{query}"), key, None)).await
}

fn principals(body: &Value) -> Vec<(String, String)> {
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["scope"]["user_id"].as_str().unwrap().to_string(),
                row["period"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn names(body: &Value) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for (principal, _) in principals(body) {
        if !seen.contains(&principal) {
            seen.push(principal);
        }
    }
    seen
}

/// Waits out a UTC day edge under 5s away (weekly and monthly edges coincide
/// with a daily one), so a charge recorded now and a request served moments
/// later share their windows.
fn clear_of_window_edge() {
    let remaining = window(Period::Daily, now()).end.saturating_sub(now());
    if remaining < 5 {
        std::thread::sleep(
            std::time::Duration::from_secs(remaining) + std::time::Duration::from_millis(50),
        );
    }
}

fn record(state: &AppState, principal: &str, femto: u64) {
    clear_of_window_edge();
    state
        .gateway_stores
        .spend
        .meter()
        .record(principal, now(), femto);
}

async fn put_limit(router: &Router, scope: Value, amount: &str, period: &str) -> Value {
    let (response, body) = post(
        router,
        WRITE_KEY,
        json!({"scope": scope, "amount": amount, "period": period}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    body
}

#[tokio::test]
async fn row_shape_for_user_cap_org_cap_and_uncapped_principals() {
    let (config, _env) = SpendEnv::config("eff-shape");
    let (router, _, state) = build_router(config).unwrap();
    let user_cap = put_limit(
        &router,
        json!({"type": "user", "user_id": "alice@example.com"}),
        "100",
        "daily",
    )
    .await;
    let org_cap = put_limit(&router, json!({"type": "organization"}), "200", "weekly").await;
    record(&state, "alice@example.com", 5 * CENT);
    record(&state, "bob@example.com", 12 * CENT + CENT / 2);
    record(&state, "carol@example.com", CENT / 1000);

    let (response, body) = get(&router, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("request-id"));
    assert_eq!(body["next_page"], Value::Null);
    assert_eq!(
        names(&body),
        ["alice@example.com", "bob@example.com", "carol@example.com"]
    );
    let rows = body["data"].as_array().unwrap();
    assert_eq!(rows.len(), 9, "one row per principal x period");

    // alice: own daily cap, no weekly/monthly cap.
    assert_eq!(
        rows[0],
        json!({
            "scope": {"type": "user", "user_id": "alice@example.com"},
            "groups": [],
            "actor": {"type": "user_actor", "user_id": "alice@example.com",
                      "name": null, "email_address": "alice@example.com", "deleted": false},
            "amount": "100",
            "currency": "USD",
            "period": "daily",
            "source": {"type": "user", "user_id": "alice@example.com"},
            "spend_limit_id": user_cap["id"],
            "period_to_date_spend": "5",
        })
    );
    assert_eq!(rows[1]["period"], "weekly");
    assert_eq!(rows[1]["amount"], "200");
    assert_eq!(rows[1]["source"], json!({"type": "organization"}));
    assert_eq!(rows[1]["spend_limit_id"], org_cap["id"]);
    // bob: org weekly cap only.
    assert_eq!(rows[3]["amount"], Value::Null);
    assert_eq!(rows[3]["source"], Value::Null);
    assert_eq!(rows[3]["spend_limit_id"], Value::Null);
    assert_eq!(rows[3]["period_to_date_spend"], "12.5");
    assert_eq!(rows[4]["amount"], "200");
    assert_eq!(rows[4]["source"], json!({"type": "organization"}));
    // carol: three-decimal spend; uncapped daily.
    assert_eq!(rows[6]["period_to_date_spend"], "0.001");
    assert_eq!(rows[6]["amount"], Value::Null);
}

#[tokio::test]
async fn an_explicit_unlimited_user_row_is_the_source_with_a_null_amount() {
    let (config, _env) = SpendEnv::config("eff-unlimited");
    let (router, _, state) = build_router(config).unwrap();
    put_limit(&router, json!({"type": "organization"}), "10", "daily").await;
    let (_, created) = post(
        &router,
        WRITE_KEY,
        json!({"scope": {"type": "user", "user_id": "vip"}, "amount": null, "period": "daily"}),
    )
    .await;
    record(&state, "vip", CENT);
    let (_, body) = get(&router, "?period[]=daily").await;
    assert_eq!(body["data"][0]["amount"], Value::Null);
    assert_eq!(body["data"][0]["source"]["user_id"], "vip");
    assert_eq!(body["data"][0]["spend_limit_id"], created["id"]);
}

#[tokio::test]
async fn actor_is_derived_from_static_token_email_and_anonymous_principals() {
    let (mut config, _env) = SpendEnv::config("eff-actor");
    let auth_env = format!("SHUNT_SPEND_TEST_AUTH_{}", std::process::id());
    std::env::set_var(&auth_env, "ci-bot:client-token-secret");
    config.server.auth = Some(InboundAuthConfig {
        jwt: Vec::new(),
        header: "x-shunt-token".to_string(),
        tokens_env: auth_env.clone(),
    });
    let (router, _, state) = build_router(config).unwrap();
    for principal in ["ci-bot", "dev@example.com", "shunt:anonymous"] {
        record(&state, principal, CENT);
    }
    let (_, body) = get(&router, "?period[]=daily").await;
    std::env::remove_var(&auth_env);
    let actors: Vec<(String, Value, Value)> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["actor"]["user_id"].as_str().unwrap().to_string(),
                row["actor"]["name"].clone(),
                row["actor"]["email_address"].clone(),
            )
        })
        .collect();
    assert_eq!(
        actors,
        [
            ("ci-bot".to_string(), json!("ci-bot"), Value::Null),
            (
                "dev@example.com".to_string(),
                Value::Null,
                json!("dev@example.com")
            ),
            ("shunt:anonymous".to_string(), Value::Null, Value::Null),
        ]
    );
}

#[tokio::test]
async fn an_opaque_principal_has_no_email_address() {
    let (config, _env) = SpendEnv::config("eff-opaque-actor");
    let (router, _, state) = build_router(config).unwrap();
    let opaque = ["auth0|abc123", "a@b", "@example.com", "x@@y.com", "a@b.co"];
    for principal in opaque {
        record(&state, principal, CENT);
    }
    let (_, body) = get(&router, "?period[]=daily").await;
    let emails: Vec<(String, Value)> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["actor"]["user_id"].as_str().unwrap().to_string(),
                row["actor"]["email_address"].clone(),
            )
        })
        .collect();
    for (user_id, email) in emails {
        if user_id == "a@b.co" {
            assert_eq!(email, json!("a@b.co"));
        } else {
            assert_eq!(email, Value::Null, "{user_id}");
        }
    }
    assert_eq!(body["data"].as_array().unwrap().len(), opaque.len());
}

#[tokio::test]
async fn user_ids_returns_exactly_those_principals_in_order_even_without_spend() {
    let (config, _env) = SpendEnv::config("eff-userids");
    let (router, _, state) = build_router(config).unwrap();
    record(&state, "a", CENT);
    record(&state, "b", CENT);
    let (response, body) = get(
        &router,
        "?user_ids[]=zed&user_ids[]=b&user_ids%5B%5D=n%40x.io&limit=1",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(names(&body), ["zed", "b", "n@x.io"]);
    assert_eq!(body["data"].as_array().unwrap().len(), 9);
    assert_eq!(body["next_page"], Value::Null);
    assert_eq!(body["data"][0]["period_to_date_spend"], "0");
}

#[tokio::test]
async fn duplicate_user_ids_yield_one_row_set_per_period() {
    let (config, _env) = SpendEnv::config("eff-userids-dup");
    let (router, _, _) = build_router(config).unwrap();
    let (response, body) = get(&router, "?user_ids[]=alice&user_ids[]=alice").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        principals(&body),
        [
            ("alice".into(), "daily".into()),
            ("alice".into(), "weekly".into()),
            ("alice".into(), "monthly".into()),
        ]
    );
}

#[tokio::test]
async fn period_filter_selects_in_given_order_and_dedupes() {
    let (config, _env) = SpendEnv::config("eff-period");
    let (router, _, state) = build_router(config).unwrap();
    record(&state, "a", CENT);
    let (_, body) = get(&router, "?period[]=monthly&period[]=daily&period[]=monthly").await;
    assert_eq!(
        principals(&body),
        [("a".into(), "monthly".into()), ("a".into(), "daily".into())]
    );
}

#[tokio::test]
async fn spend_desc_orders_by_current_spend_with_principal_ties() {
    let (config, _env) = SpendEnv::config("eff-sort");
    let (router, _, state) = build_router(config).unwrap();
    record(&state, "mid", 5 * CENT);
    record(&state, "zz-tie", 7 * CENT);
    record(&state, "aa-tie", 7 * CENT);
    record(&state, "low", CENT);
    let (_, body) = get(&router, "?sort=spend_desc&period[]=daily").await;
    assert_eq!(names(&body), ["aa-tie", "zz-tie", "mid", "low"]);
}

#[tokio::test]
async fn q_matches_case_insensitively_over_the_principal() {
    let (config, _env) = SpendEnv::config("eff-q");
    let (router, _, state) = build_router(config).unwrap();
    record(&state, "Alice@Example.com", CENT);
    record(&state, "bob@example.com", CENT);
    record(&state, "carol@corp.io", CENT);
    let (_, body) = get(&router, "?q=EXAMPLE").await;
    assert_eq!(names(&body), ["Alice@Example.com", "bob@example.com"]);
    let (_, body) = get(&router, "?q=nomatch").await;
    assert!(body["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn pagination_walks_every_principal_once_in_both_orders() {
    let (config, _env) = SpendEnv::config("eff-page");
    let (router, _, state) = build_router(config).unwrap();
    let ids = ["p1", "p2", "p3", "p4", "p5", "p6", "p7"];
    for (index, id) in ids.iter().enumerate() {
        // Pairs share a spend value so the tie-break crosses page boundaries.
        record(&state, id, (index as u64 / 2 + 1) * CENT);
    }
    for (extra, expected) in [
        ("", ids.to_vec()),
        (
            "&sort=spend_desc&period[]=daily",
            vec!["p7", "p5", "p6", "p3", "p4", "p1", "p2"],
        ),
    ] {
        let mut seen: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        let mut pages = 0;
        loop {
            let mut query = format!("?limit=3{extra}");
            if let Some(token) = &token {
                query.push_str(&format!("&page={token}"));
            }
            let (response, body) = get(&router, &query).await;
            assert_eq!(response.status(), StatusCode::OK, "{body}");
            seen.extend(names(&body));
            pages += 1;
            token = body["next_page"].as_str().map(str::to_string);
            if token.is_none() {
                break;
            }
            assert!(pages < 10, "pagination must terminate");
        }
        assert_eq!(pages, 3);
        assert_eq!(seen, expected, "extra={extra}");
    }
}

#[tokio::test]
async fn an_exactly_full_last_page_has_no_next_page() {
    let (config, _env) = SpendEnv::config("eff-exact");
    let (router, _, state) = build_router(config).unwrap();
    record(&state, "a", CENT);
    record(&state, "b", CENT);
    let (_, body) = get(&router, "?limit=2").await;
    assert_eq!(names(&body), ["a", "b"]);
    assert_eq!(body["next_page"], Value::Null);
}

#[tokio::test]
async fn invalid_queries_return_400_with_the_reference_messages_in_order() {
    let (config, _env) = SpendEnv::config("eff-400");
    let (router, _, state) = build_router(config).unwrap();
    record(&state, "a", CENT);
    let (_, token) = get(&router, "?limit=1").await;
    assert_eq!(token["next_page"], Value::Null);
    let many = "&user_ids[]=u".repeat(101);
    let long_q = "x".repeat(257);
    let cases = [
        ("?limit=0", "limit: must be between 1 and 1000"),
        ("?limit=1001", "limit: must be between 1 and 1000"),
        ("?limit=abc", "limit: must be between 1 and 1000"),
        (
            "?period[]=hourly",
            "period[]: must be one of daily, weekly, monthly",
        ),
        (
            &format!("?{}", many.trim_start_matches('&')),
            "user_ids[]: at most 100 entries per request",
        ),
        (&format!("?q={long_q}"), "q: too long"),
        ("?sort=spend_asc", "sort: must be spend_desc"),
        (
            "?sort=spend_desc",
            "sort=spend_desc requires exactly one period[]",
        ),
        (
            "?sort=spend_desc&period[]=daily&period[]=weekly",
            "sort=spend_desc requires exactly one period[]",
        ),
        ("?page=!!!notbase64", "page: invalid page token"),
        // Valid base64url JSON, but not a cursor.
        ("?page=e30", "page: invalid page token"),
        // limit is checked before period[], which is checked before sort.
        (
            "?limit=0&period[]=x&sort=bad",
            "limit: must be between 1 and 1000",
        ),
        (
            "?period[]=x&sort=bad",
            "period[]: must be one of daily, weekly, monthly",
        ),
    ];
    for (query, message) in cases {
        let (response, body) = get(&router, query).await;
        assert_error_response(&response, &body, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], message, "{query}");
    }
}

#[tokio::test]
async fn a_page_token_of_the_other_sort_kind_is_rejected() {
    let (config, _env) = SpendEnv::config("eff-kind");
    let (router, _, state) = build_router(config).unwrap();
    for id in ["a", "b", "c"] {
        record(&state, id, CENT);
    }
    let (_, asc) = get(&router, "?limit=1").await;
    let asc_token = asc["next_page"].as_str().unwrap();
    let (_, desc) = get(&router, "?limit=1&sort=spend_desc&period[]=daily").await;
    let desc_token = desc["next_page"].as_str().unwrap();
    for query in [
        format!("?sort=spend_desc&period[]=daily&page={asc_token}"),
        format!("?page={desc_token}"),
    ] {
        let (response, body) = get(&router, &query).await;
        assert_error_response(&response, &body, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["message"], "page: invalid page token");
    }
}

#[tokio::test]
async fn bad_credential_is_401_and_read_and_write_keys_are_both_accepted() {
    let (config, _env) = SpendEnv::config("eff-auth");
    let (router, _, state) = build_router(config).unwrap();
    record(&state, "a", CENT);
    let (response, body) = get_as(&router, "", "bad").await;
    assert_error_response(&response, &body, StatusCode::UNAUTHORIZED);
    // Authentication precedes query validation.
    let (response, _) = get_as(&router, "?limit=0", "bad").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    for key in [READ_KEY, WRITE_KEY] {
        let (response, body) = get_as(&router, "", key).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(names(&body), ["a"]);
    }
}

#[tokio::test]
async fn effective_is_not_shadowed_by_the_id_route_and_ids_still_resolve() {
    let (config, _env) = SpendEnv::config("eff-route");
    let (router, _, _) = build_router(config).unwrap();
    let created = put_limit(&router, json!({"type": "organization"}), "1", "daily").await;
    let (response, body) = get(&router, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body["data"].is_array(), "not a not_found_error: {body}");
    let id = created["id"].as_str().unwrap();
    let (response, body) = send(
        &router,
        request(
            "GET",
            &format!("/v1/organizations/spend_limits/{id}"),
            READ_KEY,
            None,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body["id"], id);
    let (response, _) = send(&router, request("POST", PATH, WRITE_KEY, Some(json!({})))).await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}
