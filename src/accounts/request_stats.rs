//! Per-account request counters behind the admin pool snapshot
//! (`GET /admin/api/pool`).
//!
//! One attempt is one proxied HTTP dispatch against a pool account, a
//! same-account retry included; control-plane calls on the account's
//! credential (usage polls, model discovery, catalog lookups, token
//! refreshes) are not attempts. It is classified when its response headers
//! arrive — a `2xx` status succeeded, any other status failed — or failed
//! outright when the send ends before any headers (a transport error or the
//! time-to-first-byte timeout), or cancelled when the send is dropped before
//! it resolves (the client went away, or a deadline above the adapter cut
//! it). Its latency is the time from that dispatch to its response headers,
//! so only an attempt that received headers contributes a sample. Attempts
//! over the Codex WebSocket transport, served or failed, are not counted: a
//! WebSocket turn has no per-turn response headers to classify. An HTTP
//! fallback after a failed WebSocket attempt is counted.
//!
//! The totals ride on the pool's per-identity health entry, so they share its
//! lifetime: kept across a config reload and a store rescan, dropped when the
//! account is deleted through the admin API, and reset by a restart (only
//! quota is persisted). Rows that resolve to the same account identity share
//! one set of totals. They are an account-keyed read model beside the metric
//! sinks, which never carry account identity (see `crate::metrics`).

use std::{future::Future, time::Duration};

use reqwest::StatusCode;
use tokio::time::Instant;

use super::AccountPool;
use crate::config::AccountConfig;

/// How one upstream dispatch against a pool account ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Response headers arrived `latency` after dispatch.
    Headers {
        status: StatusCode,
        latency: Duration,
    },
    /// The send ended before any response headers: a transport error or the
    /// time-to-first-byte timeout.
    BeforeHeaders,
    /// The send was dropped before it resolved: the client went away or a
    /// deadline above the adapter cut it. Neither a success nor the
    /// account's failure.
    Cancelled,
}

/// One account's attempt totals since the process started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestStats {
    attempted: u64,
    succeeded: u64,
    failed: u64,
    cancelled: u64,
    latency_total: Duration,
    latency_samples: u64,
}

impl RequestStats {
    pub(crate) fn record(&mut self, outcome: AttemptOutcome) {
        self.attempted = self.attempted.saturating_add(1);
        match outcome {
            AttemptOutcome::Headers { status, latency } => {
                if status.is_success() {
                    self.succeeded = self.succeeded.saturating_add(1);
                } else {
                    self.failed = self.failed.saturating_add(1);
                }
                self.latency_total = self.latency_total.saturating_add(latency);
                self.latency_samples = self.latency_samples.saturating_add(1);
            }
            AttemptOutcome::BeforeHeaders => self.failed = self.failed.saturating_add(1),
            AttemptOutcome::Cancelled => self.cancelled = self.cancelled.saturating_add(1),
        }
    }

    pub(crate) fn attempted(&self) -> u64 {
        self.attempted
    }

    pub(crate) fn succeeded(&self) -> u64 {
        self.succeeded
    }

    pub(crate) fn failed(&self) -> u64 {
        self.failed
    }

    pub(crate) fn cancelled(&self) -> u64 {
        self.cancelled
    }

    /// Mean time to response headers in milliseconds, or `None` before any
    /// attempt has received headers.
    pub(crate) fn mean_latency_ms(&self) -> Option<f64> {
        (self.latency_samples > 0)
            .then(|| self.latency_total.as_secs_f64() * 1000.0 / self.latency_samples as f64)
    }
}

/// Counts a send future's outcome against a pool account, in the shape of
/// `tracing`'s `.instrument(span)`:
/// `post_upstream(..).count_attempt(&state.accounts, &route.provider, account).await`.
/// The clock starts at the first poll, which is when the request is dispatched.
/// Dropping the returned future before the send resolves records the attempt
/// as [`AttemptOutcome::Cancelled`].
pub(crate) trait CountAttempt<E>:
    Future<Output = Result<reqwest::Response, E>> + Sized
{
    async fn count_attempt(
        self,
        pool: &AccountPool,
        provider: &str,
        account: &AccountConfig,
    ) -> Result<reqwest::Response, E> {
        let in_flight = InFlight {
            pool,
            provider,
            account,
            dispatched: Instant::now(),
            forget_epoch: pool.forget_epoch(),
            resolved: false,
        };
        let sent = self.await;
        let outcome = match &sent {
            Ok(response) => AttemptOutcome::Headers {
                status: response.status(),
                latency: in_flight.dispatched.elapsed(),
            },
            Err(_) => AttemptOutcome::BeforeHeaders,
        };
        in_flight.resolve(outcome);
        sent
    }
}

/// One dispatched attempt awaiting its outcome. A client disconnect or a
/// router deadline cancels the send by dropping it, and the attempt still
/// happened: the drop records it as cancelled.
struct InFlight<'a> {
    pool: &'a AccountPool,
    provider: &'a str,
    account: &'a AccountConfig,
    dispatched: Instant,
    forget_epoch: u64,
    resolved: bool,
}

impl InFlight<'_> {
    fn record(&self, outcome: AttemptOutcome) {
        // The account was forgotten (and possibly re-selected) mid-flight: the
        // entry under the key now is not the one the attempt dispatched
        // against, so the outcome records against nothing.
        if self.pool.forget_epoch() == self.forget_epoch {
            self.pool.note_attempt(self.provider, self.account, outcome);
        }
    }

    fn resolve(mut self, outcome: AttemptOutcome) {
        self.resolved = true;
        self.record(outcome);
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.resolved {
            self.record(AttemptOutcome::Cancelled);
        }
    }
}

impl<F, E> CountAttempt<E> for F where F: Future<Output = Result<reqwest::Response, E>> {}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reqwest::StatusCode;

    use super::{AttemptOutcome, CountAttempt, RequestStats};
    use crate::{
        accounts::{AccountPool, StoreFamily},
        config::AccountConfig,
    };

    fn answered(status: StatusCode, millis: u64) -> AttemptOutcome {
        AttemptOutcome::Headers {
            status,
            latency: Duration::from_millis(millis),
        }
    }

    fn account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            uuid: Some(format!("{name}-uuid")),
            store_family: Some(StoreFamily::Claude),
            ..Default::default()
        }
    }

    /// A pool that has selected `accounts`, which is what creates the entries
    /// a request's attempts then land on.
    fn selected(accounts: &[AccountConfig]) -> AccountPool {
        let pool = AccountPool::new();
        pool.select_order("anthropic", accounts, None, None, None);
        pool
    }

    #[test]
    fn record_totals_each_outcome_class() {
        let mut stats = RequestStats::default();
        stats.record(answered(StatusCode::OK, 100));
        stats.record(answered(StatusCode::SERVICE_UNAVAILABLE, 300));
        stats.record(AttemptOutcome::BeforeHeaders);
        stats.record(AttemptOutcome::Cancelled);

        assert_eq!(stats.attempted(), 4);
        assert_eq!(stats.succeeded(), 1);
        assert_eq!(stats.failed(), 2);
        assert_eq!(stats.cancelled(), 1);
        // (100 ms + 300 ms) over the two attempts that received headers.
        assert_eq!(stats.mean_latency_ms(), Some(200.0));
    }

    #[test]
    fn mean_latency_is_absent_until_an_attempt_receives_headers() {
        let mut stats = RequestStats::default();
        assert_eq!(stats.mean_latency_ms(), None);

        stats.record(AttemptOutcome::BeforeHeaders);

        assert_eq!(stats.failed(), 1);
        assert_eq!(stats.mean_latency_ms(), None);
    }

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let mut stats = RequestStats {
            attempted: u64::MAX - 1,
            succeeded: u64::MAX - 1,
            failed: u64::MAX - 1,
            cancelled: u64::MAX - 1,
            latency_total: Duration::MAX,
            latency_samples: u64::MAX - 1,
        };
        stats.record(answered(StatusCode::OK, 1));
        stats.record(answered(StatusCode::OK, 1));
        stats.record(AttemptOutcome::BeforeHeaders);
        stats.record(AttemptOutcome::BeforeHeaders);
        stats.record(AttemptOutcome::Cancelled);
        stats.record(AttemptOutcome::Cancelled);

        assert_eq!(
            stats,
            RequestStats {
                attempted: u64::MAX,
                succeeded: u64::MAX,
                failed: u64::MAX,
                cancelled: u64::MAX,
                latency_total: Duration::MAX,
                latency_samples: u64::MAX,
            }
        );
    }

    #[test]
    fn an_attempt_counts_only_against_its_own_account() {
        let (a, b) = (account("a"), account("b"));
        let pool = selected(&[a.clone(), b.clone()]);

        pool.note_attempt("anthropic", &a, answered(StatusCode::OK, 50));

        let snapshots = pool.snapshot("anthropic", &[a, b], None, None);
        assert_eq!(snapshots[0].requests_attempted, 1);
        assert_eq!(snapshots[0].requests_succeeded, 1);
        assert_eq!(snapshots[0].mean_latency_ms, Some(50.0));
        assert_eq!(snapshots[1].requests_attempted, 0);
        assert_eq!(snapshots[1].mean_latency_ms, None);
    }

    #[test]
    fn snapshot_reports_exact_totals_for_an_observed_account() {
        let a = account("a");
        let pool = selected(std::slice::from_ref(&a));
        pool.mark_healthy("anthropic", &a, true);

        pool.note_attempt("anthropic", &a, answered(StatusCode::OK, 100));
        pool.note_attempt(
            "anthropic",
            &a,
            answered(StatusCode::SERVICE_UNAVAILABLE, 300),
        );
        pool.note_attempt("anthropic", &a, AttemptOutcome::BeforeHeaders);

        let snapshot = &pool.snapshot("anthropic", &[a], None, None)[0];
        assert!(snapshot.has_state);
        assert_eq!(snapshot.requests_attempted, 3);
        assert_eq!(snapshot.requests_succeeded, 1);
        assert_eq!(snapshot.requests_failed, 2);
        assert_eq!(snapshot.mean_latency_ms, Some(200.0));
    }

    /// A TTFB timeout records the attempt and nothing else, so the row has no
    /// state yet still has to show the failure.
    #[test]
    fn snapshot_reports_totals_before_the_account_has_state() {
        let a = account("a");
        let pool = selected(std::slice::from_ref(&a));

        pool.note_attempt("anthropic", &a, AttemptOutcome::BeforeHeaders);

        let snapshot = &pool.snapshot("anthropic", &[a], None, None)[0];
        assert!(!snapshot.has_state);
        assert_eq!(snapshot.requests_attempted, 1);
        assert_eq!(snapshot.requests_failed, 1);
        assert_eq!(snapshot.mean_latency_ms, None);
    }

    #[test]
    fn forgetting_the_account_drops_its_totals() {
        let a = account("a");
        let pool = selected(std::slice::from_ref(&a));
        pool.note_attempt("anthropic", &a, answered(StatusCode::OK, 10));
        assert_eq!(
            pool.snapshot("anthropic", std::slice::from_ref(&a), None, None)[0].requests_attempted,
            1
        );

        pool.forget_identity(StoreFamily::Claude, "a-uuid", Some("a"));

        let snapshot = &pool.snapshot("anthropic", &[a], None, None)[0];
        assert_eq!(snapshot.requests_attempted, 0);
        assert_eq!(snapshot.mean_latency_ms, None);
    }

    /// An attempt still in flight when its account is deleted is discarded:
    /// it never counts against a row that no longer exists.
    #[test]
    fn an_outcome_landing_after_removal_is_dropped() {
        let a = account("a");
        let pool = selected(std::slice::from_ref(&a));
        pool.forget_identity(StoreFamily::Claude, "a-uuid", Some("a"));

        pool.note_attempt("anthropic", &a, answered(StatusCode::OK, 10));

        assert_eq!(
            pool.snapshot("anthropic", &[a], None, None)[0].requests_attempted,
            0
        );
    }

    #[tokio::test]
    async fn a_send_that_fails_before_headers_counts_without_a_latency_sample() {
        let a = account("a");
        let pool = selected(std::slice::from_ref(&a));
        let send = std::future::ready(Err::<reqwest::Response, _>("connection refused"));

        let sent = send.count_attempt(&pool, "anthropic", &a).await;

        assert_eq!(sent.err(), Some("connection refused"));
        let snapshot = &pool.snapshot("anthropic", &[a], None, None)[0];
        assert_eq!(snapshot.requests_attempted, 1);
        assert_eq!(snapshot.requests_failed, 1);
        assert_eq!(snapshot.mean_latency_ms, None);
    }

    /// The wrapper's own clock: a send answering 250 ms after its first poll
    /// is one success with exactly that latency.
    #[tokio::test(start_paused = true)]
    async fn the_wrapper_times_a_send_to_its_response_headers() {
        let a = account("a");
        let pool = selected(std::slice::from_ref(&a));
        let send = async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok::<_, &str>(reqwest::Response::from(axum::http::Response::new(
                Vec::<u8>::new(),
            )))
        };

        let sent = send.count_attempt(&pool, "anthropic", &a).await;

        assert_eq!(sent.map(|response| response.status()), Ok(StatusCode::OK));
        let snapshot = &pool.snapshot("anthropic", &[a], None, None)[0];
        assert_eq!(snapshot.requests_attempted, 1);
        assert_eq!(snapshot.requests_succeeded, 1);
        assert_eq!(snapshot.mean_latency_ms, Some(250.0));
    }

    /// A send dropped before it resolves (here by a deadline above the
    /// wrapper, as a client disconnect would) still counts, as cancelled: no
    /// failure, no latency sample.
    #[tokio::test(start_paused = true)]
    async fn a_send_dropped_before_it_resolves_counts_as_cancelled() {
        let a = account("a");
        let pool = selected(std::slice::from_ref(&a));
        let send = std::future::pending::<Result<reqwest::Response, &str>>();

        let cut = tokio::time::timeout(
            Duration::from_millis(20),
            send.count_attempt(&pool, "anthropic", &a),
        )
        .await;

        assert!(cut.is_err(), "the deadline cut the send");
        let snapshot = &pool.snapshot("anthropic", &[a], None, None)[0];
        assert_eq!(snapshot.requests_attempted, 1);
        assert_eq!(snapshot.requests_succeeded, 0);
        assert_eq!(snapshot.requests_failed, 0);
        assert_eq!(snapshot.requests_cancelled, 1);
        assert_eq!(snapshot.mean_latency_ms, None);
    }

    /// An attempt whose account is forgotten mid-flight records against
    /// nothing, even when the same identity is re-selected before the send
    /// resolves: the recreated entry is not the one the attempt dispatched
    /// against, and removal is meant to reset the totals.
    #[tokio::test(start_paused = true)]
    async fn an_attempt_dispatched_before_a_forget_lands_on_no_replacement() {
        let a = account("a");
        let pool = std::sync::Arc::new(selected(std::slice::from_ref(&a)));
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let driver = tokio::spawn({
            let pool = pool.clone();
            let a = a.clone();
            async move {
                let send = async move {
                    let _ = release_rx.await;
                    Err::<reqwest::Response, ()>(())
                };
                send.count_attempt(&pool, "anthropic", &a).await
            }
        });
        // The dispatch (and its capture) ran before the forget below.
        tokio::time::sleep(Duration::from_millis(20)).await;

        pool.forget_identity(StoreFamily::Claude, "a-uuid", Some("a"));
        pool.select_order("anthropic", std::slice::from_ref(&a), None, None, None);
        let _ = release.send(());
        let sent = driver.await.unwrap();
        assert!(sent.is_err(), "the gated send answers Err");

        let snapshot = &pool.snapshot("anthropic", std::slice::from_ref(&a), None, None)[0];
        assert_eq!(snapshot.requests_attempted, 0);
        assert_eq!(snapshot.requests_failed, 0);
    }

    /// The guard above only spans a forget: an attempt dispatched over a
    /// stable entry records normally.
    #[tokio::test(start_paused = true)]
    async fn an_attempt_dispatched_over_a_stable_entry_still_records() {
        let a = account("a");
        let pool = std::sync::Arc::new(selected(std::slice::from_ref(&a)));
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let driver = tokio::spawn({
            let pool = pool.clone();
            let a = a.clone();
            async move {
                let send = async move {
                    let _ = release_rx.await;
                    Err::<reqwest::Response, ()>(())
                };
                send.count_attempt(&pool, "anthropic", &a).await
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let _ = release.send(());
        let sent = driver.await.unwrap();
        assert!(sent.is_err(), "the gated send answers Err");

        let snapshot = &pool.snapshot("anthropic", std::slice::from_ref(&a), None, None)[0];
        assert_eq!(snapshot.requests_attempted, 1);
        assert_eq!(snapshot.requests_failed, 1);
    }
}
