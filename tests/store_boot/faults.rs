//! The proxy path never waits on the store (SC-004, NFR-001).
//!
//! Faults are injected at the engine, with no hook in the binary:
//! - SQLite: this test process opens the same database file and holds a write
//!   lock (`BEGIN EXCLUSIVE` / `BEGIN IMMEDIATE`), so every write the binary
//!   attempts waits out its 5 s busy timeout.
//! - PostgreSQL: "unavailable" terminates the binary's server connections
//!   (found by the `application_name` the harness tags them with) and drops
//!   the test's schema, so every later statement fails; "slow" installs
//!   statement-level triggers on `pool_quota` and `instance_hold` (the hold
//!   row every durable write updates first) that `pg_sleep(3)`.

use std::time::{Duration, Instant};

use reqwest::StatusCode;

use crate::harness::{self, Engine, Fixture, SqliteLock};
use crate::{per_engine, ALL};

/// The pool persister's timer period (`state_persist::FLUSH_INTERVAL`).
const FLUSH_INTERVAL: Duration = Duration::from_secs(15);
/// The binary's SQLite busy timeout (`store::open::SQLITE_BUSY_TIMEOUT`).
const SQLITE_BUSY: Duration = Duration::from_secs(5);
const DELAY: Duration = Duration::from_secs(3);
const REQUESTS: usize = 50;

async fn sleep_until(at: Instant) {
    tokio::time::sleep_until(at.into()).await;
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

/// SC-004: with the store gone mid-run, 50 proxied requests all succeed.
/// The shutdown that follows proves the binary really lost the store: its
/// final pool write fails (or runs out of time) instead of landing.
async fn store_unavailable(engine: Engine) {
    let fixture = Fixture::new("unavailable", engine).await;
    let client = client();
    let mut shunt = fixture.boot(ALL).await;
    // Dirty the pool so shutdown has something to write.
    assert_eq!(
        harness::post_message(&client, &fixture).await.status(),
        StatusCode::OK
    );

    let lock = match &fixture.engine {
        Engine::Sqlite => Some(SqliteLock::hold(&fixture.path("store.db"), "EXCLUSIVE")),
        #[cfg(feature = "store-postgres")]
        Engine::Postgres(scratch) => {
            scratch.terminate_binary_connections().await;
            scratch.drop_schema().await;
            None
        }
    };
    for index in 0..REQUESTS {
        let response = harness::post_message(&client, &fixture).await;
        assert_eq!(response.status(), StatusCode::OK, "request {index}");
    }

    let status = shunt.terminate().await;
    if let Some(lock) = lock {
        lock.release();
    }
    let log = shunt.log();
    assert!(status.success(), "{status}:\n{log}");
    assert!(
        log.contains("failed to persist pool state to the store")
            || log.contains("final store flush failed at shutdown")
            || log.contains("store shutdown did not finish in time"),
        "the final write must have hit the unavailable store:\n{log}"
    );
    fixture.finish().await;
}
per_engine!(
    proxied_requests_succeed_with_the_store_unavailable,
    super::store_unavailable
);

#[cfg(feature = "store-postgres")]
const DELAY_TRIGGERS: &str = "\
CREATE FUNCTION store_boot_delay() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM pg_sleep(3); RETURN NULL; END $$;
CREATE TRIGGER delay_pool BEFORE INSERT OR UPDATE OR DELETE ON pool_quota
  FOR EACH STATEMENT EXECUTE FUNCTION store_boot_delay();
CREATE TRIGGER delay_hold BEFORE INSERT OR UPDATE OR DELETE ON instance_hold
  FOR EACH STATEMENT EXECUTE FUNCTION store_boot_delay();";

#[cfg(feature = "store-postgres")]
const DROP_DELAY_TRIGGERS: &str = "\
DROP TRIGGER delay_pool ON pool_quota; DROP TRIGGER delay_hold ON instance_hold;";

/// NFR-001: while the pool persister's write is held by the store for at
/// least 3 s, 50 proxied requests each finish in under 1 s.
///
/// The persister's first write tick lands one interval after it logs that
/// it started (the test reads that line's timestamp). The pool is dirtied
/// right after boot, the delay is armed before the earliest tick, and the requests start
/// after the latest one. The store still has no pool row once all 50 are
/// done (the write is in progress), and gains it shortly after the delay
/// lifts, long before the next tick: that row is the write the requests ran
/// alongside.
async fn slow_store(engine: Engine) {
    let fixture = Fixture::new("slow", engine).await;
    let client = client();
    let mut shunt = fixture.boot(ALL).await;
    // The persister's ticker is created right after this line, and its first
    // write tick fires one interval later.
    let enabled = shunt.logged_at("pool state persistence enabled (store)");
    let earliest_tick = enabled + FLUSH_INTERVAL;
    let latest_tick = earliest_tick + Duration::from_millis(250);
    assert_eq!(
        harness::post_message(&client, &fixture).await.status(),
        StatusCode::OK
    );
    assert_eq!(fixture.rows("pool_quota").await, 0);

    let lock = match &fixture.engine {
        Engine::Sqlite => {
            // Released at `latest_tick + DELAY`, inside the write's
            // `SQLITE_BUSY` wait, so the held write then lands.
            const _: () = assert!(DELAY.as_millis() + 250 < SQLITE_BUSY.as_millis());
            sleep_until(earliest_tick - Duration::from_secs(2)).await;
            Some(SqliteLock::hold(&fixture.path("store.db"), "IMMEDIATE"))
        }
        #[cfg(feature = "store-postgres")]
        Engine::Postgres(scratch) => {
            scratch.scoped(DELAY_TRIGGERS).await;
            None
        }
    };

    sleep_until(latest_tick + Duration::from_millis(300)).await;
    let mut slowest = Duration::ZERO;
    for index in 0..REQUESTS {
        let started = Instant::now();
        let response = harness::post_message(&client, &fixture).await;
        let took = started.elapsed();
        assert_eq!(response.status(), StatusCode::OK, "request {index}");
        assert!(
            took < Duration::from_secs(1),
            "request {index} took {took:?}"
        );
        slowest = slowest.max(took);
    }
    println!("slowest of {REQUESTS} proxied requests: {slowest:?}");
    assert_eq!(
        fixture.rows("pool_quota").await,
        0,
        "the pool write must still be held by the store after the requests"
    );

    if let Some(lock) = lock {
        sleep_until(latest_tick + DELAY).await;
        lock.release();
    }
    let deadline = earliest_tick + FLUSH_INTERVAL - Duration::from_secs(1);
    while fixture.rows("pool_quota").await == 0 {
        assert!(
            Instant::now() < deadline,
            "the held pool write never landed before the next tick:\n{}",
            shunt.log()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let landed = Instant::now();
    assert!(
        landed >= earliest_tick + DELAY,
        "the write landed {:?} after the earliest tick; it was not held for 3 s",
        landed - earliest_tick
    );
    #[cfg(feature = "store-postgres")]
    if let Engine::Postgres(scratch) = &fixture.engine {
        scratch.scoped(DROP_DELAY_TRIGGERS).await;
    }
    assert!(shunt.terminate().await.success(), "{}", shunt.log());
    fixture.finish().await;
}
per_engine!(
    proxied_requests_stay_fast_while_the_store_holds_the_pool_write,
    super::slow_store
);
