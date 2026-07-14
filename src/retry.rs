//! Bounded, idempotent upstream retry with exponential backoff + jitter.
//!
//! [`send_with_retry`] wraps a *single upstream attempt* — the step that
//! obtains the response status and headers (`.send()` on a `reqwest` request, a
//! Cursor connect frame) — and re-issues it on a clearly transient failure. It
//! is deliberately positioned *before* any response body is streamed to the
//! client: the caller only starts relaying bytes downstream after this layer
//! has already returned a response, so a retry can never happen mid-stream. Once
//! a byte reaches the client the request is committed and unrecoverable, which
//! is why this operates on the pre-stream boundary alone (issue #48).
//!
//! Retries are scoped to *transient* outcomes only:
//!
//! - transient statuses `429`, `502`, `503`, `504`, and `529` — Anthropic's
//!   non-standard "Overloaded", a transient signal like the others (never any
//!   other `4xx` — a `400`/`401`/`403`/`404`/`413` is a request problem an
//!   identical retry cannot fix — nor `500`, frequently a deterministic
//!   upstream bug);
//! - connection-level transport errors (connect refused/reset, timeout) that
//!   resolve before any response body exists.
//!
//! Which of those two a call retries depends on [`RetrySafety`] (issue #126).
//! A response *status* is only retried on an *idempotent* call. For a
//! non-idempotent creation POST — Anthropic Messages and the single-credential
//! Responses path, wired as [`RetrySafety::NonIdempotentPost`] — any response
//! status is ambiguous: it means the upstream may already have accepted the
//! request and started a billable generation, so re-issuing could duplicate it.
//! Those paths therefore retry a pre-response *transport* error only and surface
//! a transient status immediately. A transport error carries no such ambiguity
//! (nothing was accepted before it resolved), so it is retried for both
//! safeties. Cursor's `Run` is also a non-idempotent POST but still retries on
//! transient status for now: it has no account-pool failover, so a surfaced
//! blip would hit the client directly, and it has no stable idempotency identity
//! yet — tightening it is tracked inline as `TODO(#126, cursor)`. The shared
//! client configures no response/read timeout, so `is_timeout()` only fires
//! connect-phase (before acceptance); retries are capped by `max_retries`.
//!
//! Backoff is exponential with true randomized (full) jitter, capped at
//! [`RetryPolicy::max_backoff`]. A server-supplied `Retry-After` takes
//! precedence when present; if it asks for a wait longer than `max_backoff`,
//! the attempt is surfaced immediately rather than slept past budget (the
//! `exceeds-budget` behavior). The pooled OAuth failover paths
//! (`forward_claude_oauth` / `forward_chatgpt_oauth`) have their own
//! account-rotation resilience and deliberately do *not* layer this on top; it
//! applies to the single-credential upstream calls that otherwise surface a
//! transient blip straight to the client.

use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use rand::Rng;

/// A bounded retry policy derived from `[providers.<name>.retry]`. `Copy` so it
/// threads cheaply from the request-config snapshot to the upstream call site.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    /// Additional attempts after the first. `0` disables retry entirely.
    pub max_retries: u32,
    /// Backoff ceiling for the first retry, grown by `multiplier` per attempt.
    pub initial_backoff: Duration,
    /// Upper bound on any single backoff and on an honored `Retry-After`.
    pub max_backoff: Duration,
    /// Exponential growth factor applied per attempt (>= 1.0).
    pub multiplier: f64,
}

impl RetryPolicy {
    /// A policy that never retries — used for `count_tokens` and whenever a
    /// provider sets `max_retries = 0`.
    pub const DISABLED: RetryPolicy = RetryPolicy {
        max_retries: 0,
        initial_backoff: Duration::from_millis(0),
        max_backoff: Duration::from_millis(0),
        multiplier: 1.0,
    };

    /// Whether this policy will ever issue a retry.
    pub fn is_enabled(&self) -> bool {
        self.max_retries > 0
    }
}

/// Transient upstream statuses that an identical retry can plausibly clear.
/// Includes `529` (Anthropic's non-standard "Overloaded"). Deliberately
/// excludes every other `4xx` (a request-level error) and `500` (which is
/// frequently a deterministic upstream bug, not a blip).
pub fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 502 | 503 | 504 | 529)
}

/// A failure to obtain an upstream response. Implemented per adapter error type
/// so the driver can decide, without knowing the concrete error, whether the
/// failure is a transient transport problem worth retrying.
pub trait RetryableError {
    /// `true` for a connection-level transport failure (connect/reset/timeout)
    /// that occurred before any response body existed; `false` for a
    /// deterministic error (bad request build, decode) an identical retry
    /// cannot fix.
    fn is_transient(&self) -> bool;
}

/// Controls whether a response status may be retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetrySafety {
    /// The operation can safely be repeated after a response status.
    Idempotent,
    /// The operation is a creation POST. A response means the upstream may
    /// already have accepted it, so only pre-response transport errors retry.
    NonIdempotentPost,
}

impl RetrySafety {
    fn may_retry_response_status(self) -> bool {
        matches!(self, Self::Idempotent)
    }
}

impl RetryableError for reqwest::Error {
    fn is_transient(&self) -> bool {
        // A `.send()` future resolves once response headers arrive, so any error
        // it yields is pre-body. Retry the clearly transient kinds; a
        // builder/redirect/decode error is deterministic and left alone.
        self.is_connect() || self.is_timeout()
    }
}

/// The backoff decision for one retryable outcome.
enum Backoff {
    /// Sleep this long, then re-attempt.
    Sleep(Duration),
    /// Give up now and surface the outcome — the server asked us to wait longer
    /// than `max_backoff`.
    ExceedsBudget,
}

/// Drive one upstream request through a bounded retry loop with
/// [`RetrySafety::Idempotent`] — a response status *and* a pre-response
/// transport error both retry. `attempt` is re-invoked from scratch (fresh
/// request, full body) on each try. Returns the first non-retryable outcome, or
/// the last outcome once retries are exhausted.
///
/// Prefer [`send_with_retry_with_safety`] with the call's real [`RetrySafety`]
/// stated explicitly. This idempotent-default wrapper is retained for Cursor's
/// `Run`, itself a non-idempotent POST that still retries on a response status
/// pending a stable idempotency identity (`TODO(#126, cursor)`).
///
/// The success type is always [`reqwest::Response`]: every adapter obtains a
/// `reqwest::Response` on success and only its status/headers are inspected here
/// (never its body), so retrying stays strictly pre-stream.
pub async fn send_with_retry<E, F, Fut>(
    policy: RetryPolicy,
    provider: &str,
    attempt: F,
) -> Result<reqwest::Response, E>
where
    E: RetryableError + std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, E>>,
{
    send_with_retry_with_safety(policy, provider, RetrySafety::Idempotent, attempt).await
}

/// Drive one upstream request through a bounded retry loop with an explicit
/// acceptance-safety policy. Non-idempotent POSTs only retry transport errors
/// returned before response headers, because any response status is ambiguous:
/// the upstream may already have started a billable generation.
pub async fn send_with_retry_with_safety<E, F, Fut>(
    policy: RetryPolicy,
    provider: &str,
    safety: RetrySafety,
    mut attempt: F,
) -> Result<reqwest::Response, E>
where
    E: RetryableError + std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, E>>,
{
    let mut retries: u32 = 0;
    loop {
        let outcome = attempt().await;
        let retries_left = retries < policy.max_retries;

        match outcome {
            Ok(response)
                if safety.may_retry_response_status()
                    && retries_left
                    && is_retryable_status(response.status()) =>
            {
                let status = response.status();
                match next_backoff(&policy, retries, Some(response.headers())) {
                    Backoff::Sleep(delay) => {
                        // Release the upstream connection before backing off:
                        // `response` owns the body stream tied to the socket, so
                        // holding it across the sleep would pin that connection
                        // idle for the whole delay (up to `max_backoff`), adding
                        // connection churn under a retry storm — the exact scenario
                        // this feature exists for. The `ExceedsBudget` arm keeps it,
                        // since it surfaces the response instead of sleeping.
                        drop(response);
                        tracing::warn!(
                            provider = %provider,
                            status = status.as_u16(),
                            attempt = retries + 1,
                            max_retries = policy.max_retries,
                            delay_ms = delay.as_millis(),
                            "retrying transient upstream response"
                        );
                        crate::metrics::record_upstream_retry(provider, retry_reason(status));
                        tokio::time::sleep(delay).await;
                        retries += 1;
                    }
                    // Server asked for a wait past our budget: surface the
                    // response now and let the client decide, rather than hang.
                    // Log it — this "upstream needs longer than we'll wait" signal
                    // is operationally interesting, and without it an operator has
                    // no trace the give-up path was even reached.
                    Backoff::ExceedsBudget => {
                        tracing::warn!(
                            provider = %provider,
                            status = status.as_u16(),
                            max_backoff_ms = policy.max_backoff.as_millis(),
                            "surfacing transient upstream response without retry: Retry-After exceeds max backoff"
                        );
                        return Ok(response);
                    }
                }
            }
            Err(error) if retries_left && error.is_transient() => {
                // Transport errors carry no `Retry-After`, so always back off.
                let delay = match next_backoff(&policy, retries, None) {
                    Backoff::Sleep(delay) => delay,
                    Backoff::ExceedsBudget => unreachable!("no headers ⇒ no Retry-After"),
                };
                tracing::warn!(
                    provider = %provider,
                    error = %error,
                    attempt = retries + 1,
                    max_retries = policy.max_retries,
                    delay_ms = delay.as_millis(),
                    "retrying transient upstream transport error"
                );
                crate::metrics::record_upstream_retry(provider, "transport");
                tokio::time::sleep(delay).await;
                retries += 1;
            }
            // Retries exhausted (or a non-retryable outcome): log why we stopped —
            // the per-attempt warnings never mark the loop finally stopping — then
            // hand the outcome back. Gated on an enabled policy so a DISABLED one
            // (max_retries == 0, e.g. count_tokens), which lands here having
            // exhausted nothing, is not mislogged as "gave up after exhausting
            // retries".
            other if policy.max_retries > 0 => {
                log_terminal_outcome(provider, &policy, safety, &other);
                return other;
            }
            // Non-retryable outcome, or a disabled policy: hand it back untouched.
            other => return other,
        }
    }
}

/// Emit the single "why we stopped" WARN for a terminal outcome the driver hands
/// back after its retry budget (split out of the loop to keep its control flow
/// flat). A no-op unless the outcome is still a transient failure.
fn log_terminal_outcome<E: RetryableError + std::fmt::Display>(
    provider: &str,
    policy: &RetryPolicy,
    safety: RetrySafety,
    outcome: &Result<reqwest::Response, E>,
) {
    match outcome {
        // A retryable status reaches here for one of two reasons, and conflating
        // them would misread the logs. On an idempotent call the retry budget was
        // genuinely exhausted; on a non-idempotent POST the status was never
        // eligible to retry (`safety.may_retry_response_status()` is false), so it
        // surfaced on the first attempt without any retry — logging "exhausted
        // retries" there would send an operator chasing a backoff loop that never
        // ran.
        Ok(response)
            if safety.may_retry_response_status() && is_retryable_status(response.status()) =>
        {
            tracing::warn!(
                provider = %provider,
                status = response.status().as_u16(),
                max_retries = policy.max_retries,
                "giving up after exhausting retries: upstream still returning a transient status"
            );
        }
        Ok(response) if is_retryable_status(response.status()) => {
            tracing::warn!(
                provider = %provider,
                status = response.status().as_u16(),
                "surfacing transient upstream response without retry: non-idempotent request may already be accepted upstream"
            );
        }
        Err(error) if error.is_transient() => {
            tracing::warn!(
                provider = %provider,
                error = %error,
                max_retries = policy.max_retries,
                "giving up after exhausting retries: upstream transport error persists"
            );
        }
        _ => {}
    }
}

/// A concise, low-cardinality reason label for the retry metric.
fn retry_reason(status: StatusCode) -> &'static str {
    match status.as_u16() {
        429 => "429",
        502 => "502",
        503 => "503",
        504 => "504",
        529 => "529",
        _ => "other",
    }
}

/// Decide how long to wait before the next attempt. An honored `Retry-After`
/// wins over computed backoff; when it exceeds `max_backoff` the caller gives up
/// cleanly instead of sleeping past budget.
fn next_backoff(policy: &RetryPolicy, attempt: u32, headers: Option<&HeaderMap>) -> Backoff {
    if let Some(retry_after) = headers.and_then(crate::accounts::retry_after) {
        if retry_after > policy.max_backoff {
            return Backoff::ExceedsBudget;
        }
        return Backoff::Sleep(retry_after);
    }
    Backoff::Sleep(jittered_backoff(policy, attempt))
}

/// The exponential backoff ceiling for `attempt` (0-based), capped at
/// `max_backoff`. Pure — the randomized jitter is applied separately so this
/// stays deterministically testable.
fn backoff_ceiling(policy: &RetryPolicy, attempt: u32) -> Duration {
    let base = policy.initial_backoff.as_secs_f64() * policy.multiplier.powi(attempt as i32);
    let capped = base.min(policy.max_backoff.as_secs_f64());
    Duration::from_secs_f64(capped.max(0.0))
}

/// Full jitter: a uniform random wait in `[0, ceiling]`. Randomized (not the
/// fixed fraction some proxies use) so concurrent retriers don't resynchronize.
fn jittered_backoff(policy: &RetryPolicy, attempt: u32) -> Duration {
    let ceiling = backoff_ceiling(policy, attempt).as_secs_f64();
    if ceiling <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(rand::rng().random_range(0.0..=ceiling))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(1000),
            multiplier: 2.0,
        }
    }

    #[test]
    fn only_transient_statuses_are_retryable() {
        // 529 is Anthropic's non-standard "Overloaded" — retryable like the rest.
        for code in [429, 502, 503, 504, 529] {
            assert!(is_retryable_status(StatusCode::from_u16(code).unwrap()));
        }
        for code in [200, 400, 401, 403, 404, 413, 500, 501] {
            assert!(!is_retryable_status(StatusCode::from_u16(code).unwrap()));
        }
    }

    #[test]
    fn retry_reason_labels_each_transient_status_distinctly() {
        // Each retryable status maps to its own low-cardinality metric label; a
        // transposed match arm (e.g. 502/504 swapped) would be caught here.
        assert_eq!(retry_reason(StatusCode::from_u16(429).unwrap()), "429");
        assert_eq!(retry_reason(StatusCode::from_u16(502).unwrap()), "502");
        assert_eq!(retry_reason(StatusCode::from_u16(503).unwrap()), "503");
        assert_eq!(retry_reason(StatusCode::from_u16(504).unwrap()), "504");
        assert_eq!(retry_reason(StatusCode::from_u16(529).unwrap()), "529");
        // Anything else collapses to the catch-all label.
        assert_eq!(retry_reason(StatusCode::from_u16(500).unwrap()), "other");
    }

    #[test]
    fn disabled_policy_never_retries() {
        assert!(!RetryPolicy::DISABLED.is_enabled());
        assert!(!policy_with_zero_retries().is_enabled());
        assert!(policy().is_enabled());
    }

    fn policy_with_zero_retries() -> RetryPolicy {
        RetryPolicy {
            max_retries: 0,
            ..policy()
        }
    }

    #[test]
    fn backoff_ceiling_grows_then_saturates() {
        let policy = policy();
        assert_eq!(backoff_ceiling(&policy, 0), Duration::from_millis(100));
        assert_eq!(backoff_ceiling(&policy, 1), Duration::from_millis(200));
        assert_eq!(backoff_ceiling(&policy, 2), Duration::from_millis(400));
        // 100 * 2^4 = 1600ms, capped at the 1000ms ceiling.
        assert_eq!(backoff_ceiling(&policy, 4), Duration::from_millis(1000));
    }

    #[test]
    fn jitter_stays_within_ceiling() {
        let policy = policy();
        for attempt in 0..5 {
            let ceiling = backoff_ceiling(&policy, attempt);
            for _ in 0..64 {
                assert!(jittered_backoff(&policy, attempt) <= ceiling);
            }
        }
    }

    #[test]
    fn retry_after_within_budget_is_honored() {
        // 1s == the 1000ms max_backoff: at the inclusive boundary it is honored.
        let policy = policy();
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "1".parse().unwrap());
        match next_backoff(&policy, 0, Some(&headers)) {
            Backoff::Sleep(delay) => assert_eq!(delay, Duration::from_secs(1)),
            Backoff::ExceedsBudget => {
                panic!("a 1s Retry-After at the budget boundary must be slept")
            }
        }
    }

    #[test]
    fn retry_after_over_budget_gives_up() {
        // max_backoff is 1000ms; a 5s Retry-After exceeds it and must give up
        // rather than sleep past budget.
        let policy = policy();
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "5".parse().unwrap());
        assert!(matches!(
            next_backoff(&policy, 0, Some(&headers)),
            Backoff::ExceedsBudget
        ));
    }

    #[test]
    fn cursor_error_transient_classification_follows_status() {
        // A transient status is retried; a request error is not. Uses the same
        // status set as HTTP responses, exercised here via a stand-in impl.
        struct Stub(u16);
        impl RetryableError for Stub {
            fn is_transient(&self) -> bool {
                is_retryable_status(StatusCode::from_u16(self.0).unwrap())
            }
        }
        assert!(Stub(503).is_transient());
        assert!(!Stub(400).is_transient());
    }

    // --- Driver behavior --------------------------------------------------
    //
    // These exercise `send_with_retry` end to end with in-memory
    // `reqwest::Response`s and stub errors, so every branch (status retry,
    // transport-error retry, non-transient short-circuit, exceeds-budget give
    // up, exhaustion) is covered without a network round-trip.

    /// A near-zero backoff so the loop never actually sleeps in tests.
    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            multiplier: 2.0,
        }
    }

    fn response(status: u16) -> reqwest::Response {
        reqwest::Response::from(
            axum::http::Response::builder()
                .status(status)
                .body(reqwest::Body::from("x"))
                .unwrap(),
        )
    }

    fn response_with_retry_after(status: u16, seconds: &str) -> reqwest::Response {
        reqwest::Response::from(
            axum::http::Response::builder()
                .status(status)
                .header("retry-after", seconds)
                .body(reqwest::Body::from("x"))
                .unwrap(),
        )
    }

    #[derive(Debug)]
    struct StubError {
        transient: bool,
    }

    impl std::fmt::Display for StubError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "stub error (transient={})", self.transient)
        }
    }

    impl RetryableError for StubError {
        fn is_transient(&self) -> bool {
            self.transient
        }
    }

    #[tokio::test]
    async fn driver_retries_transient_status_then_returns_success() {
        let mut calls = 0u32;
        let result: Result<reqwest::Response, StubError> =
            send_with_retry(fast_policy(), "test", || {
                calls += 1;
                let n = calls;
                async move {
                    if n < 3 {
                        Ok(response(503))
                    } else {
                        Ok(response(200))
                    }
                }
            })
            .await;
        assert_eq!(result.unwrap().status().as_u16(), 200);
        assert_eq!(calls, 3, "two retries after the first attempt");
    }

    #[tokio::test]
    async fn non_idempotent_post_does_not_retry_after_response_headers() {
        let mut calls = 0u32;
        let result: Result<reqwest::Response, StubError> = send_with_retry_with_safety(
            fast_policy(),
            "test",
            RetrySafety::NonIdempotentPost,
            || {
                calls += 1;
                async move { Ok(response(503)) }
            },
        )
        .await;

        assert_eq!(result.unwrap().status().as_u16(), 503);
        assert_eq!(calls, 1, "a response means the POST may have been accepted");
    }

    #[tokio::test]
    async fn non_idempotent_post_still_retries_transport_error() {
        // The acceptance-safety gate suppresses only *response-status* retries.
        // A pre-response transport error resolves before the upstream accepts
        // anything, so it stays unambiguous and must still retry even for a
        // non-idempotent POST — the positive half of the #126 invariant.
        let mut calls = 0u32;
        let result = send_with_retry_with_safety(
            fast_policy(),
            "test",
            RetrySafety::NonIdempotentPost,
            || {
                calls += 1;
                let n = calls;
                async move {
                    if n == 1 {
                        Err(StubError { transient: true })
                    } else {
                        Ok(response(200))
                    }
                }
            },
        )
        .await;

        assert_eq!(result.unwrap().status().as_u16(), 200);
        assert_eq!(calls, 2, "a pre-response transport error still retries");
    }

    #[tokio::test]
    async fn driver_retries_transient_transport_error_then_succeeds() {
        let mut calls = 0u32;
        let result = send_with_retry(fast_policy(), "test", || {
            calls += 1;
            let n = calls;
            async move {
                if n == 1 {
                    Err(StubError { transient: true })
                } else {
                    Ok(response(200))
                }
            }
        })
        .await;
        assert_eq!(result.unwrap().status().as_u16(), 200);
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn driver_does_not_retry_non_transient_error() {
        let mut calls = 0u32;
        let result: Result<reqwest::Response, StubError> =
            send_with_retry(fast_policy(), "test", || {
                calls += 1;
                async move { Err(StubError { transient: false }) }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(calls, 1, "a non-transient error is surfaced immediately");
    }

    #[tokio::test]
    async fn driver_gives_up_when_retry_after_exceeds_budget() {
        // max_backoff is 2ms; a 5s Retry-After exceeds it, so the 503 is
        // surfaced immediately rather than slept past budget.
        let mut calls = 0u32;
        let result: Result<reqwest::Response, StubError> =
            send_with_retry(fast_policy(), "test", || {
                calls += 1;
                async move { Ok(response_with_retry_after(503, "5")) }
            })
            .await;
        assert_eq!(result.unwrap().status().as_u16(), 503);
        assert_eq!(calls, 1, "exceeds-budget Retry-After skips the retry");
    }

    #[tokio::test]
    async fn driver_surfaces_last_response_after_exhausting_retries() {
        let mut calls = 0u32;
        let result: Result<reqwest::Response, StubError> =
            send_with_retry(fast_policy(), "test", || {
                calls += 1;
                async move { Ok(response(503)) }
            })
            .await;
        assert_eq!(result.unwrap().status().as_u16(), 503);
        assert_eq!(calls, 4, "one initial attempt plus max_retries (3)");
    }

    #[tokio::test]
    async fn real_reqwest_connect_error_is_transient() {
        // Drive a genuine `reqwest::Error` (connection refused on port 1) through
        // the production `RetryableError` impl, not a stub — this is the exact
        // classification the Anthropic/Responses adapters rely on for issue #48's
        // "connection errors", and guards against an inverted boolean or a reqwest
        // upgrade changing what `is_connect()` reports.
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("connecting to port 1 must fail");
        assert!(error.is_connect());
        assert!(error.is_transient());
    }

    #[tokio::test]
    async fn disabled_policy_makes_a_single_attempt() {
        let mut calls = 0u32;
        let result: Result<reqwest::Response, StubError> =
            send_with_retry(RetryPolicy::DISABLED, "test", || {
                calls += 1;
                async move { Ok(response(503)) }
            })
            .await;
        assert_eq!(result.unwrap().status().as_u16(), 503);
        assert_eq!(calls, 1);
    }
}
