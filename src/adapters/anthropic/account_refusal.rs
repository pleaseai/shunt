//!
//! Three upstream answers mean the whole account (not the request) is out of
//! room: the organisation or workspace spend limit, an exhausted credit
//! balance, and any `billing_error`. Retrying the same account is pointless
//! until the cause clears, so the caller skips it for a window. The spend-limit
//! message states when access returns; the other two carry no time.
//!
//! Matching is on the start of the documented message, never a substring: a
//! validation 400 that merely mentions `usage limits` must stay terminal.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use axum::http::StatusCode;

use crate::{
    accounts::epoch_secs_after,
    adapters::{AdapterError, ResponseBounds},
    auth::claude::usage::parse_rfc3339_to_epoch_secs,
    config::AccountConfig,
    error::ERROR_ENVELOPE_BUDGET,
    routing::Route,
    server::AppState,
};

/// Skip window when the refusal states no usable resume time.
pub(crate) const REFUSAL_DEFAULT_SKIP: Duration = Duration::from_secs(60 * 60);

const SPEND_LIMIT_PREFIXES: [&str; 2] = [
    "You have reached your specified API usage limits",
    "You have reached your specified workspace API usage limits",
];
const CREDIT_BALANCE_PREFIX: &str = "Your credit balance is too low to access the Anthropic API";
const RESUME_MARKER: &str = "regain access on ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusalKind {
    SpendLimit,
    CreditBalance,
    Billing,
}

impl RefusalKind {
    /// The stable label the chain logs and the skip state records.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::SpendLimit => "spend_limit",
            Self::CreditBalance => "credit_balance",
            Self::Billing => "billing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AccountRefusal {
    pub(crate) kind: RefusalKind,
    /// Stated access-resume instant, seconds since the Unix epoch (spend limit only).
    pub(crate) resume_at_epoch_secs: Option<u64>,
}

impl AccountRefusal {
    /// How long to skip the account: until the stated resume time when it is
    /// still ahead of `now`, otherwise [`REFUSAL_DEFAULT_SKIP`].
    pub(crate) fn skip_window(&self, now: SystemTime) -> Duration {
        let now_secs = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        match self.resume_at_epoch_secs {
            Some(resume) if resume > now_secs => Duration::from_secs(resume - now_secs),
            _ => REFUSAL_DEFAULT_SKIP,
        }
    }
}

/// Judge a relayed upstream response for an account-level refusal before it
/// reaches the client. Only a 400 or 402 has its body read, under the shared
/// error-body bounds; every other status returns untouched and unread. The
/// returned response carries the upstream's body intact (a 400 or 402 in a
/// supported content coding comes back decoded). A body that exceeds the
/// smaller of the caller's cap (`bounds.max_bytes`) and the envelope
/// ceiling, or is not the Anthropic envelope, is not classified as a refusal.
///
/// Without an idle clock (`bounds.idle = None`), a body that outlasts
/// [`ERROR_ENVELOPE_BUDGET`] is passed on unjudged. With an idle clock, an idle
/// gap that ends before the budget, or a budget trip before any body bytes
/// arrive, returns the idle error instead. A budget trip after bytes arrive
/// still passes the body on unjudged. The idle clock starts at header arrival,
/// as the relay's own body read starts it, and each chunk refreshes it.
pub(super) async fn judge(
    upstream: reqwest::Response,
    bounds: ResponseBounds,
) -> Result<(reqwest::Response, Option<AccountRefusal>), AdapterError> {
    let clock = bounds.idle.map(|idle| (idle, tokio::time::Instant::now()));
    let (upstream, refused) =
        judge_body(upstream, clock, ERROR_ENVELOPE_BUDGET, bounds.max_bytes).await?;
    Ok((upstream, refused.map(|(refusal, _)| refusal)))
}

/// Only a 400 or 402 body can be an account-level refusal.
fn can_refuse(status: u16) -> bool {
    status == 400 || status == 402
}

/// The status guard, bounded read, and classification [`judge`] and [`peek`]
/// share, returning the refusal with the body it was judged on.
async fn judge_body(
    upstream: reqwest::Response,
    clock: Option<(Duration, tokio::time::Instant)>,
    budget: Duration,
    cap: Option<usize>,
) -> Result<(reqwest::Response, Option<(AccountRefusal, bytes::Bytes)>), AdapterError> {
    let status = upstream.status().as_u16();
    if !can_refuse(status) {
        return Ok((upstream, None));
    }
    // `normalize` drops the declared length, so check it first: a body declared
    // over the limit is passed on unread, as `buffer_error_body_capped` passes it.
    let declared_oversized = upstream
        .content_length()
        .is_some_and(|length| length > crate::adapters::error_body_limit(cap) as u64);
    // Judge the envelope, not its content coding.
    let upstream = crate::response_coding::normalize(upstream);
    if declared_oversized {
        return Ok((upstream, None));
    }
    let (upstream, body) =
        crate::adapters::buffer_error_body_capped(upstream, clock, true, budget, cap).await?;
    let refused = body.and_then(|body| classify(status, &body).map(|refusal| (refusal, body)));
    Ok((upstream, refused))
}

/// Record a refusal `route`'s upstream answered: start its skip window unless
/// the route is passthrough (the caller's own credential, never skipped), and
/// log it once. Called where the refusal is classified, on both chain paths.
pub(super) fn note(state: &AppState, route: &Route, status: StatusCode, refusal: AccountRefusal) {
    let skips =
        (!state.config.route_is_passthrough(route)).then(|| state.accounts.upstream_skips());
    crate::proxy::failover::record_refusal(
        skips,
        &route.provider,
        &route.model,
        status,
        refusal.kind.label(),
        refusal.skip_window(SystemTime::now()),
    );
}

/// The pool-rotation reason a refusal records against a Claude pool account,
/// beside [`crate::accounts::rotation_reason`]'s status-derived labels.
pub(super) const POOL_REFUSAL_REASON: &str = "account_refusal";

/// Judge a Claude pool account's relayed answer (`forward_claude_oauth`'s
/// Relay arms, first attempt and post-refresh retry). A refusal cools the
/// account for the refusal's window so selection skips it — the pool's
/// per-account cooldown is its skip state, so no upstream skip is recorded —
/// and logs once. The refusal proves the bearer authenticated, so a stale
/// needs-re-login mark is cleared as a relayed 4xx clears it; the cooldown is
/// left standing, unlike the healthy mark a relayed answer gets. `Some` is the
/// response to relay; `None` is a refusal, whose response is stored in
/// `last_response` as the pool's fallback if every account refuses.
pub(super) async fn judge_pool_answer(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
    upstream: reqwest::Response,
    bounds: ResponseBounds,
    last_response: &mut Option<reqwest::Response>,
) -> Result<Option<reqwest::Response>, AdapterError> {
    let status = upstream.status();
    let (upstream, refused) = judge(upstream, bounds).await?;
    let Some(refusal) = refused else {
        return Ok(Some(upstream));
    };
    let window = refusal.skip_window(SystemTime::now());
    let standing =
        state
            .accounts
            .cooldown_refusal(&route.provider, account, window, POOL_REFUSAL_REASON);
    state.accounts.clear_needs_relogin(&route.provider, account);
    let cooldown_until = epoch_secs_after(standing);
    tracing::warn!(
        provider = %route.provider,
        account = %account.name,
        status = status.as_u16(),
        refusal = refusal.kind.label(),
        cooldown_until_epoch_secs = cooldown_until,
        "Claude OAuth account refused the request at the account level; cooling it down and rotating to the next account"
    );
    *last_response = Some(upstream);
    Ok(None)
}

/// Skip a Claude pool upstream that every account refused, so multi-element
/// chains stop paying one refused round trip per account on each later
/// request (#817 FR-6). Selection still offers cooling accounts as a last
/// resort, so without this the exhausted pool is contacted again every time.
/// The window ends when the first account becomes selectable again; with
/// none known it is [`REFUSAL_DEFAULT_SKIP`]. A pool with an account
/// selectable right now (one that did not refuse and is not cooling) is not
/// skipped. Each refusal was already logged where it was seen; this logs the
/// distinct pool-level skip once.
pub(super) fn skip_refused_pool(state: &AppState, route: &Route, accounts: &[AccountConfig]) {
    let window = match state.accounts.earliest_reselect(
        &route.provider,
        accounts,
        state.config.server.pool.as_ref(),
    ) {
        Some(window) if window.is_zero() => return,
        Some(window) => window,
        None => REFUSAL_DEFAULT_SKIP,
    };
    let lasts = state
        .accounts
        .upstream_skips()
        .record(&route.provider, window);
    let skip_until = epoch_secs_after(lasts);
    tracing::warn!(
        provider = %route.provider,
        model = %route.model,
        skip_until_epoch_secs = skip_until,
        "Claude OAuth pool exhausted on account refusals; skipping the upstream until an account can serve again"
    );
}

/// How long the committed streaming chain (`chain_attempt`) reads a 400 or 402
/// body before it decides without it. That chain decides at header arrival,
/// so the peek must stay short: a body that stalls past it falls back to the
/// status-only terminal answer, and must do so well inside the one-second
/// bound the stalled-400 test holds `chain_attempt` to. An Anthropic refusal
/// is a small JSON body sent with its headers, so it arrives within the peek.
pub(super) const CHAIN_PEEK_BUDGET: Duration = Duration::from_millis(250);

/// The committed streaming chain's form of [`judge`]: read a 400 or 402 body
/// under [`CHAIN_PEEK_BUDGET`] and no idle clock (that path has none). The
/// response comes back rebuilt with the upstream's body intact, read or not
/// (decoded as [`judge`] decodes it),
/// and with the refusal plus the body it was judged on when it is one.
/// Every other status returns untouched and unread.
pub(super) async fn peek(
    upstream: reqwest::Response,
) -> Result<(reqwest::Response, Option<(AccountRefusal, bytes::Bytes)>), AdapterError> {
    judge_body(upstream, None, CHAIN_PEEK_BUDGET, None).await
}

/// Classify an upstream error response; `None` means it is not an account-level refusal.
pub(crate) fn classify(status: u16, body: &[u8]) -> Option<AccountRefusal> {
    if !can_refuse(status) {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let error = value.get("error")?;
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    let error_type = error.get("type").and_then(Value::as_str);

    if status == 402 && error_type == Some("billing_error") {
        return Some(AccountRefusal {
            kind: RefusalKind::Billing,
            resume_at_epoch_secs: None,
        });
    }
    if message.starts_with(CREDIT_BALANCE_PREFIX) {
        return Some(AccountRefusal {
            kind: RefusalKind::CreditBalance,
            resume_at_epoch_secs: None,
        });
    }
    if status == 400 && SPEND_LIMIT_PREFIXES.iter().any(|p| message.starts_with(p)) {
        return Some(AccountRefusal {
            kind: RefusalKind::SpendLimit,
            resume_at_epoch_secs: parse_resume(message),
        });
    }
    None
}

/// Parse `YYYY-MM-DD at HH:MM UTC` following `regain access on `.
fn parse_resume(message: &str) -> Option<u64> {
    let rest = &message[message.find(RESUME_MARKER)? + RESUME_MARKER.len()..];
    let (date, rest) = rest.split_once(" at ")?;
    let (clock, _) = rest.split_once(" UTC")?;
    if date.len() != 10 || clock.len() != 5 {
        return None;
    }
    if !date_exists(date) {
        return None;
    }
    parse_rfc3339_to_epoch_secs(&format!("{date}T{clock}:00Z"))
}

/// Whether `YYYY-MM-DD` names a real calendar day. The shared RFC 3339 parser
/// only range-checks the day against 31, so `2026-02-31` would roll over.
fn date_exists(date: &str) -> bool {
    let (Some(year), Some(month), Some(day)) = (
        date.get(0..4).and_then(|y| y.parse::<u32>().ok()),
        date.get(5..7).and_then(|m| m.parse::<u32>().ok()),
        date.get(8..10).and_then(|d| d.parse::<u32>().ok()),
    ) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(error_type: &str, message: &str) -> Vec<u8> {
        serde_json::json!({"type": "error", "error": {"type": error_type, "message": message}})
            .to_string()
            .into_bytes()
    }

    const RESUME: u64 = 1_793_491_200; // 2026-11-01T00:00:00Z

    fn spend_msg(prefix: &str) -> String {
        format!("{prefix}. You will regain access on 2026-11-01 at 00:00 UTC.")
    }

    fn kind(status: u16, body: &[u8]) -> Option<RefusalKind> {
        classify(status, body).map(|r| r.kind)
    }

    #[test]
    fn spend_limit_org_and_workspace_classify_with_resume_time() {
        for prefix in SPEND_LIMIT_PREFIXES {
            let body = envelope("invalid_request_error", &spend_msg(prefix));
            let refusal = classify(400, &body).expect("spend limit");
            assert_eq!(refusal.kind, RefusalKind::SpendLimit);
            assert_eq!(refusal.resume_at_epoch_secs, Some(RESUME));
        }
    }

    #[test]
    fn spend_limit_needs_status_400() {
        let body = envelope("invalid_request_error", &spend_msg(SPEND_LIMIT_PREFIXES[0]));
        assert_eq!(kind(400, &body), Some(RefusalKind::SpendLimit));
        assert_eq!(kind(402, &body), None);
        assert_eq!(kind(429, &body), None);
    }

    #[test]
    fn credit_balance_classifies_on_400_and_402() {
        let body = envelope(
            "invalid_request_error",
            "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing.",
        );
        assert_eq!(kind(400, &body), Some(RefusalKind::CreditBalance));
        assert_eq!(kind(402, &body), Some(RefusalKind::CreditBalance));
        assert_eq!(kind(500, &body), None);
    }

    #[test]
    fn billing_error_classifies_on_any_402_message_only() {
        let body = envelope("billing_error", "anything at all");
        assert_eq!(kind(402, &body), Some(RefusalKind::Billing));
        assert_eq!(kind(400, &body), None);
    }

    #[test]
    fn usage_limits_substring_and_validation_400_stay_terminal() {
        let substring = envelope(
            "invalid_request_error",
            "max_tokens exceeds the usage limits of this model",
        );
        assert_eq!(kind(400, &substring), None);
        let validation = envelope("invalid_request_error", "messages: field required");
        assert_eq!(kind(400, &validation), None);
        // Positive twin: the real prefix on the same status is a refusal.
        let real = envelope("invalid_request_error", &spend_msg(SPEND_LIMIT_PREFIXES[0]));
        assert!(classify(400, &real).is_some());
    }

    #[test]
    fn non_json_html_bom_and_missing_fields_are_not_refusals() {
        let good = envelope("invalid_request_error", &spend_msg(SPEND_LIMIT_PREFIXES[0]));
        assert!(classify(400, &good).is_some());
        assert_eq!(classify(400, b"<html>Bad Request</html>"), None);
        assert_eq!(classify(400, b"not json"), None);
        assert_eq!(classify(400, b""), None);
        assert_eq!(classify(400, br#"{"type":"error"}"#), None);
        assert_eq!(classify(400, br#"{"error":{"type":"x"}}"#), None);
        // serde_json rejects a UTF-8 BOM prefix, so such a body stays terminal.
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(&good);
        assert_eq!(classify(400, &bom), None);
    }

    /// The top-level `"type": "error"` envelope is required: the same messages
    /// without it, or under another type, are not refusals.
    #[test]
    fn a_body_without_the_error_envelope_type_is_not_a_refusal() {
        let bodies = [
            (
                400,
                "invalid_request_error",
                spend_msg(SPEND_LIMIT_PREFIXES[0]),
            ),
            (
                400,
                "invalid_request_error",
                "Your credit balance is too low to access the Anthropic API.".to_string(),
            ),
            (402, "billing_error", "Payment required.".to_string()),
        ];
        for (status, error_type, message) in bodies {
            let error = serde_json::json!({"type": error_type, "message": message});
            let typed = serde_json::json!({"type": "error", "error": error}).to_string();
            assert!(classify(status, typed.as_bytes()).is_some(), "{status}");
            for outer in [
                serde_json::json!({"error": error}),
                serde_json::json!({"type": "message", "error": error}),
            ] {
                assert_eq!(classify(status, outer.to_string().as_bytes()), None);
            }
        }
    }

    #[test]
    fn skip_window_uses_future_resume_else_one_hour() {
        let now = UNIX_EPOCH + Duration::from_secs(RESUME - 600);
        let future = AccountRefusal {
            kind: RefusalKind::SpendLimit,
            resume_at_epoch_secs: Some(RESUME),
        };
        assert_eq!(future.skip_window(now), Duration::from_secs(600));

        let past = AccountRefusal {
            resume_at_epoch_secs: Some(RESUME - 1200),
            ..future
        };
        assert_eq!(past.skip_window(now), REFUSAL_DEFAULT_SKIP);
        let missing = AccountRefusal {
            resume_at_epoch_secs: None,
            ..future
        };
        assert_eq!(missing.skip_window(now), REFUSAL_DEFAULT_SKIP);
    }

    /// Credit-balance and billing refusals state no resume time, so they skip
    /// for the one-hour default (#817 AC-011).
    #[test]
    fn credit_balance_and_billing_refusals_skip_for_one_hour() {
        let credit = envelope(
            "invalid_request_error",
            "Your credit balance is too low to access the Anthropic API.",
        );
        let billing = envelope("billing_error", "Payment required.");
        let now = UNIX_EPOCH + Duration::from_secs(RESUME);
        for (status, body) in [(400, &credit), (402, &credit), (402, &billing)] {
            let refusal = classify(status, body).expect("a refusal");
            assert_eq!(
                refusal.skip_window(now),
                Duration::from_secs(60 * 60),
                "{status} {:?}",
                refusal.kind
            );
        }
    }

    #[test]
    fn unreadable_resume_time_yields_default_window() {
        for tail in [
            "",
            " You will regain access on soon.",
            " You will regain access on 2026-13-45 at 99:99 UTC.",
            " You will regain access on 2026-11-01 at 00:00.",
        ] {
            let msg = format!("{}.{tail}", SPEND_LIMIT_PREFIXES[0]);
            let refusal = classify(400, &envelope("invalid_request_error", &msg)).unwrap();
            assert_eq!(refusal.resume_at_epoch_secs, None, "{tail}");
            assert_eq!(refusal.skip_window(UNIX_EPOCH), REFUSAL_DEFAULT_SKIP);
        }
    }
    /// A stated date that does not exist is unreadable, not rolled over to the
    /// next month; real leap days are accepted.
    #[test]
    fn impossible_calendar_dates_yield_default_window() {
        let resume = |date: &str| {
            let msg = format!(
                "{}. You will regain access on {date} at 00:00 UTC.",
                SPEND_LIMIT_PREFIXES[0]
            );
            classify(400, &envelope("invalid_request_error", &msg))
                .unwrap()
                .resume_at_epoch_secs
        };
        for date in [
            "2026-02-31",
            "2026-04-31",
            "2026-02-29",
            "2026-00-10",
            "2026-01-00",
        ] {
            assert_eq!(resume(date), None, "{date}");
        }
        assert_eq!(resume("2028-02-29"), Some(1_835_395_200));
        assert_eq!(resume("2026-11-01"), Some(RESUME));
    }

    /// A refusal-shaped 400 whose body is longer than `len` bytes.
    fn padded_refusal(len: usize) -> String {
        let message = format!("{}. {}", SPEND_LIMIT_PREFIXES[0], "x".repeat(len));
        String::from_utf8(envelope("invalid_request_error", &message)).unwrap()
    }

    async fn upstream_response(body: String) -> reqwest::Response {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_raw(body, "application/json"))
            .mount(&server)
            .await;
        reqwest::Client::new()
            .post(server.uri())
            .send()
            .await
            .unwrap()
    }

    fn capped(max_bytes: usize) -> ResponseBounds {
        ResponseBounds {
            max_bytes: Some(max_bytes),
            idle: None,
        }
    }

    /// A body longer than the caller's cap is not judged, whatever it says:
    /// the relay that follows still owes the caller its too-large error. The
    /// body stays whole for that relay to count.
    #[tokio::test]
    async fn a_refusal_body_over_the_caller_cap_is_not_judged() {
        let body = padded_refusal(500);
        let (relayed, refusal) = judge(upstream_response(body.clone()).await, capped(100))
            .await
            .unwrap();
        assert_eq!(refusal, None);
        assert_eq!(relayed.text().await.unwrap(), body);

        let under = padded_refusal(0);
        let (_, refusal) = judge(upstream_response(under).await, capped(4096))
            .await
            .unwrap();
        assert!(refusal.is_some(), "the same body under the cap is judged");
    }

    /// A 400 that declares more than the cap and then stalls is passed on at
    /// once, not held for the envelope read's budget.
    #[tokio::test]
    async fn a_declared_oversized_stalled_body_is_not_waited_for() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 400 Bad\r\ncontent-type: application/json\r\ncontent-length: 4096\r\n\r\n{\"type\":",
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let upstream = reqwest::Client::new()
            .post(format!("http://{addr}"))
            .send()
            .await
            .unwrap();

        let judged = tokio::time::timeout(Duration::from_secs(1), judge(upstream, capped(100)))
            .await
            .expect("a declared-oversized body is not read");

        assert_eq!(judged.unwrap().1, None);
    }

    /// On a pool path the same rule keeps an oversized refusal-prefixed body
    /// from rotating the pool: it is relayed (to meet the too-large error),
    /// and the account is not cooled.
    #[tokio::test]
    async fn an_oversized_refusal_does_not_rotate_the_pool() {
        let config = crate::config::Config::default();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let route = Route {
            provider: "anthropic".to_string(),
            adapter: crate::routing::AdapterKind::Anthropic,
            model: "claude-test".to_string(),
            upstream_model: "claude-test".to_string(),
            effort: None,
            service_tier: None,
        };
        let account = AccountConfig {
            name: "a".to_string(),
            token_env: Some("SHUNT_TEST_REFUSAL_CAP_A".to_string()),
            ..Default::default()
        };
        let upstream = upstream_response(padded_refusal(500)).await;

        let answer = judge_pool_answer(&state, &route, &account, upstream, capped(100), &mut None)
            .await
            .unwrap();

        assert!(answer.is_some());
        assert_eq!(
            state
                .accounts
                .earliest_reselect("anthropic", std::slice::from_ref(&account), None),
            Some(Duration::ZERO),
            "an unjudged body leaves the account selectable"
        );
    }
}
