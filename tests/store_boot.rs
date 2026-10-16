//! `shunt run` with a `[store]` (ADR-0006, T009), driven as a real binary and
//! run once per engine: SQLite always, PostgreSQL with `store-postgres` and
//! `SHUNT_TEST_POSTGRES_URL` (skipped when unset, a failure when
//! `SHUNT_TEST_POSTGRES_REQUIRED=1`; each test gets its own schema).

use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::Value;

mod common;
#[path = "store_boot/faults.rs"]
mod faults;
#[path = "store_boot/harness.rs"]
mod harness;

use harness::{Engine, Fixture, Sections, ADMIN_TOKEN, HOSTNAME, SEEDED_REFRESH, UTILIZATION};

const ALL: Sections = Sections {
    gateway: true,
    spend: true,
};

/// Define `mod $name { sqlite, postgres }`, each calling `$body(engine)`.
macro_rules! per_engine {
    ($name:ident, $body:path) => {
        mod $name {
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn sqlite() {
                $body(crate::harness::Engine::Sqlite).await;
            }

            #[cfg(feature = "store-postgres")]
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn postgres() {
                let Some(scratch) = crate::harness::postgres::Scratch::new().await else {
                    return;
                };
                $body(crate::harness::Engine::Postgres(scratch)).await;
            }
        }
    };
}
pub(crate) use per_engine;

async fn rotate(client: &reqwest::Client, base_url: &str, refresh: &str) -> (StatusCode, Value) {
    let response = client
        .post(format!("{base_url}/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", "claude-code"),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .expect("token request");
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn create_spend_limit(client: &reqwest::Client, base_url: &str, amount: &str) {
    let response = client
        .post(format!("{base_url}/v1/organizations/spend_limits"))
        .header("x-api-key", ADMIN_TOKEN)
        .json(&serde_json::json!({
            "scope": {"type": "organization"},
            "amount": amount,
            "period": "monthly",
        }))
        .send()
        .await
        .expect("spend request");
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
}

async fn spend_limits(client: &reqwest::Client, base_url: &str) -> String {
    let response = client
        .get(format!("{base_url}/v1/organizations/spend_limits"))
        .header("x-api-key", ADMIN_TOKEN)
        .send()
        .await
        .expect("spend list");
    assert_eq!(response.status(), StatusCode::OK);
    response.text().await.unwrap()
}

async fn pool_utilization(client: &reqwest::Client, base_url: &str) -> Option<f64> {
    let response = client
        .get(format!("{base_url}/admin/api/pool"))
        .header("x-shunt-admin-token", ADMIN_TOKEN)
        .send()
        .await
        .expect("pool snapshot");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    body["providers"]
        .as_array()?
        .iter()
        .find(|table| table["provider"] == "anthropic")?["accounts"]
        .as_array()?
        .iter()
        .find(|account| account["name"] == "store-a")?["utilization_5h"]
        .as_f64()
}

/// AC-001 / SC-001, plus graceful shutdown (AC-024): pool quota, gateway
/// sessions, and spend limits survive a restart. The restart follows the
/// SIGTERM immediately, so it only boots if shutdown released the hold, and
/// the pool quota only survives if shutdown flushed it (the persister's 15 s
/// timer never fires in this window; asserted below).
async fn restart_keeps_state(engine: Engine) {
    let fixture = Fixture::new("restart", engine).await;
    fixture.seed_snapshots();
    let client = reqwest::Client::new();
    let mut first = fixture.boot(ALL).await;
    let first_tick =
        first.logged_at("pool state persistence enabled (store)") + Duration::from_secs(15);
    let base = first.base_url.clone();

    let response = harness::post_message(&client, &fixture).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(pool_utilization(&client, &base).await, Some(UTILIZATION));
    create_spend_limit(&client, &base, "123").await;
    let (status, body) = rotate(&client, &base, SEEDED_REFRESH).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rotated = body["refresh_token"].as_str().unwrap().to_string();
    assert_eq!(fixture.rows("pool_quota").await, 0, "no timer flush yet");

    assert!(
        Instant::now() + Duration::from_secs(1) < first_tick,
        "inconclusive: the 15 s persister tick may flush instead of shutdown"
    );
    let status = first.terminate().await;
    assert!(status.success(), "{status}:\n{}", first.log());
    assert_eq!(fixture.rows("pool_quota").await, 1, "{}", first.log());
    assert!(
        first.log().contains("store hold released and store closed"),
        "{}",
        first.log()
    );

    let mut second = fixture.boot(ALL).await;
    assert_eq!(pool_utilization(&client, &base).await, Some(UTILIZATION));
    assert!(spend_limits(&client, &base).await.contains("123"));
    let (status, body) = rotate(&client, &base, &rotated).await;
    assert_eq!(status, StatusCode::OK, "the rotated token survives: {body}");
    // The original token is a tombstone: replaying it is still refused.
    let (status, _) = rotate(&client, &base, SEEDED_REFRESH).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(second.terminate().await.success(), "{}", second.log());
    fixture.finish().await;
}
per_engine!(
    restart_keeps_pool_sessions_and_spend,
    super::restart_keeps_state
);

/// AC-002: with a store, the configured snapshots are import sources only.
/// Absent ones are never created by boot, traffic, or shutdown.
async fn absent_snapshots_stay_absent(engine: Engine) {
    let fixture = Fixture::new("absent", engine).await;
    let client = reqwest::Client::new();
    let mut shunt = fixture.boot(ALL).await;
    let base = shunt.base_url.clone();
    assert_eq!(
        harness::post_message(&client, &fixture).await.status(),
        StatusCode::OK
    );
    create_spend_limit(&client, &base, "7").await;
    assert!(shunt.terminate().await.success(), "{}", shunt.log());
    for path in [
        fixture.pool_snapshot(),
        fixture.gateway_snapshot(),
        fixture.spend_snapshot(),
    ] {
        assert!(!path.exists(), "{} was created", path.display());
    }
    // The writes went somewhere: the store.
    assert_eq!(fixture.rows("spend_limit").await, 1);
    assert_eq!(fixture.rows("pool_quota").await, 1);
    fixture.finish().await;
}
per_engine!(
    absent_snapshots_are_never_created,
    super::absent_snapshots_stay_absent
);

/// AC-002: existing snapshots are imported, then stay byte-identical through
/// boot, pool/spend/gateway writes, and shutdown.
async fn present_snapshots_stay_identical(engine: Engine) {
    let fixture = Fixture::new("present", engine).await;
    fixture.seed_snapshots();
    let paths = [
        fixture.pool_snapshot(),
        fixture.gateway_snapshot(),
        fixture.spend_snapshot(),
    ];
    let before: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let client = reqwest::Client::new();
    let mut shunt = fixture.boot(ALL).await;
    let base = shunt.base_url.clone();
    assert_eq!(
        harness::post_message(&client, &fixture).await.status(),
        StatusCode::OK
    );
    create_spend_limit(&client, &base, "9").await;
    let (status, body) = rotate(&client, &base, SEEDED_REFRESH).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(shunt.terminate().await.success(), "{}", shunt.log());
    for (path, bytes) in paths.iter().zip(&before) {
        assert_eq!(
            &std::fs::read(path).unwrap(),
            bytes,
            "{} changed",
            path.display()
        );
    }
    // Imported (one session, rotated into two rows) and written to the store.
    assert_eq!(fixture.rows("import_record").await, 3);
    assert_eq!(fixture.rows("refresh_token").await, 2);
    assert_eq!(fixture.rows("spend_limit").await, 1);
    fixture.finish().await;
}
per_engine!(
    present_snapshots_stay_byte_identical,
    super::present_snapshots_stay_identical
);

/// T005 at the binary level (AC-026): a second `shunt run` on a held store
/// exits non-zero and names the first process's holder label.
async fn second_instance_refused(engine: Engine) {
    let fixture = Fixture::new("second", engine).await;
    let mut first = fixture.boot(ALL).await;
    let other = fixture.config(harness::free_port(), ALL);
    let mut second = fixture.spawn(&other);
    let status = second.wait_exit(Duration::from_secs(30)).await;
    assert!(!status.success());
    let log = second.log();
    assert!(log.contains("held by another shunt instance"), "{log}");
    assert!(
        log.contains(&format!("{HOSTNAME}:{}", first.pid())),
        "{log}"
    );
    assert!(first.terminate().await.success(), "{}", first.log());
    fixture.finish().await;
}
per_engine!(
    a_second_instance_on_a_held_store_exits_naming_the_first,
    super::second_instance_refused
);

/// AC-019/AC-025: an unreachable store refuses to boot and names the host.
#[cfg(feature = "store-postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_postgres_store_exits_non_zero_naming_the_host() {
    let fixture = Fixture::new("unreachable", Engine::Sqlite).await;
    let config = fixture.path("unreachable.toml");
    std::fs::write(
        &config,
        format!(
            "[server]\nbind = \"127.0.0.1:{}\"\n\n[store]\nbackend = \"postgres\"\n\
             url = \"postgres://shunt@127.0.0.1:1/shunt?sslmode=disable\"\n",
            fixture.port
        ),
    )
    .unwrap();
    let mut shunt = fixture.spawn(&config);
    let status = shunt.wait_exit(Duration::from_secs(60)).await;
    assert!(!status.success());
    let log = shunt.log();
    assert!(log.contains("failed to start the store"), "{log}");
    assert!(log.contains("127.0.0.1:1"), "{log}");
    assert!(log.contains("unreachable"), "{log}");
    fixture.finish().await;
}
