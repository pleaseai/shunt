//! Background poller for upstream provider Statuspage `summary.json`
//! endpoints (`[server.status]`).
//!
//! Observation-only: this module's only effect is to update
//! [`crate::upstream_status::StatusStore`], which feeds the
//! `shunt.upstream.status` metric and the admin dashboard's "Upstream status"
//! strip. It is never consulted by routing, failover, or pool/cooldown
//! decisions.
//!
//! Mirrors [`crate::usage_poll::spawn_usage_poller`]'s shape: a `spawn_*`
//! entry point that is a no-op when unconfigured, `tokio::time::interval`
//! with `MissedTickBehavior::Skip`, and a loop that never terminates. Every
//! per-source failure is folded into an `Unknown` entry by
//! [`crate::upstream_status::UpstreamStatus::unknown`] rather than escaping
//! the poll — one bad or hostile source cannot stop the others from polling,
//! crash the task, or leave a stale "operational" value in the store.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{future::join_all, StreamExt};

use crate::{
    config::StatusSource,
    metrics,
    server::AppState,
    upstream_status::{parse_summary, UpstreamStatus},
};

/// Bound on a `summary.json` response body. Real Statuspage summaries are a
/// few KB even with several incidents listed; 1 MiB is generous headroom
/// while still bounding the memory cost of a misbehaving or malicious
/// endpoint. Enforced while streaming the body in, not after buffering it in
/// full — see [`read_bounded_body`].
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Per-request timeout for a status fetch. The shared `state.http_client`
/// carries no default timeout, so every outbound call must set its own
/// (mirrors `claude::usage::fetch_usage`'s 10s).
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Spawn the status poller if `[server.status]` enables it: present, with a
/// non-empty `sources` list, and a nonzero effective interval. A no-op
/// otherwise, so the default deployment adds no background work. Whether the
/// task exists is decided once from the boot config (like the usage poller);
/// a reload does not start or stop it.
pub fn spawn_status_poller(state: AppState) {
    let Some(status) = state.config.server.status.as_ref() else {
        return;
    };
    if status.sources.is_empty() {
        return;
    }
    let Some(interval) = status.refresh_interval() else {
        return;
    };
    // The interval floor is applied silently in config; surface the clamp so
    // an operator who set e.g. 30 isn't left wondering why the log below
    // shows 60.
    if status.refresh_seconds != 0 && status.refresh_seconds != interval {
        tracing::warn!(
            configured_seconds = status.refresh_seconds,
            effective_seconds = interval,
            "server.status.refresh_seconds is below the 60s floor; using 60"
        );
    }
    tracing::info!(
        interval_seconds = interval,
        sources = status.sources.len(),
        "starting upstream status poller"
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval));
        // A poll that runs long (or a suspend/resume) must not make the
        // ticker fire a burst of catch-up ticks. Skip missed ticks and
        // resume on the regular cadence.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // `interval` fires its first tick immediately, so the store is seeded
        // at startup and then refreshed every `interval` seconds.
        loop {
            ticker.tick().await;
            poll_all(&state).await;
        }
    });
}

/// Poll every configured source once. Re-snapshots the live shared state so a
/// reloaded `[server.status]` is picked up on the next tick, like
/// [`crate::usage_poll`]'s poller. Each source is independent: one source's
/// failure cannot stop the others from polling or block the tick.
async fn poll_all(state: &AppState) {
    let state = state.refreshed();
    let Some(status) = state.config.server.status.as_ref() else {
        for removed in state.status.retain_providers(std::iter::empty()) {
            metrics::record_upstream_status(&removed, None);
        }
        return;
    };
    let providers = status.sources.iter().map(|source| source.provider.as_str());
    for removed in state.status.retain_providers(providers) {
        metrics::record_upstream_status(&removed, None);
    }
    let client = &state.http_client;
    let polls = status.sources.iter().map(|source| async move {
        let observed = poll_source(client, source).await;
        (source, observed)
    });
    for (source, observed) in join_all(polls).await {
        metrics::record_upstream_status(&source.provider, observed.indicator.severity());
        tracing::debug!(
            provider = source.provider,
            indicator = ?observed.indicator,
            "status poller: applied observed status"
        );
        state.status.set(&source.provider, observed);
    }
}

/// Fetch and parse one source's `summary.json`. Never panics and never
/// propagates an error: every failure mode (transport error, non-2xx,
/// oversized body, invalid JSON, unrecognized indicator string) is folded
/// into `Indicator::Unknown` so a failed poll can only ever replace the
/// stored entry with "no signal" — never leave a stale "operational" value in
/// place, and never silently report "operational" for a source shunt could
/// not actually reach.
async fn poll_source(client: &reqwest::Client, source: &StatusSource) -> UpstreamStatus {
    let observed_at = now_unix();
    let response = match client
        .get(source.url.trim())
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return UpstreamStatus::unknown(
                observed_at,
                format!("request failed: {}", error.without_url()),
            )
        }
    };
    let status_code = response.status();
    let body = match read_bounded_body(response, MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(message) => return UpstreamStatus::unknown(observed_at, message),
    };
    if !status_code.is_success() {
        return UpstreamStatus::unknown(observed_at, format_non_2xx_detail(status_code, &body));
    }
    parse_summary(&body, observed_at)
}

/// Build the stored error detail for a non-2xx status response.
///
/// The status line (`status request failed (403 Forbidden)`) is always
/// present. An excerpt of `body` is appended only when the body itself looks
/// like a diagnostic — not markup: a Cloudflare (or similar) block page in
/// front of a status endpoint returns an HTML error page, and that page's
/// raw markup has no diagnostic value but, rendered verbatim into the admin
/// dashboard's Description column, makes the "Upstream status" table look
/// broken. A JSON or plain-text error body, by contrast, is worth keeping.
///
/// When an excerpt is appended, whitespace runs (including newlines) are
/// collapsed to single spaces first so the stored detail is always one line,
/// then capped at ~120 chars — comfortably under `UpstreamStatus::unknown`'s
/// 200-char backstop, which still applies to every error path including this
/// one.
fn format_non_2xx_detail(status_code: reqwest::StatusCode, body: &str) -> String {
    let base = format!("status request failed ({status_code})");
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return base;
    }
    let collapsed = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    let excerpt: String = collapsed.chars().take(120).collect();
    format!("{base}: {excerpt}")
}

/// Read a response body as UTF-8 text, capped at `limit` bytes. The body is
/// rejected outright once it exceeds the cap — not truncated and parsed
/// anyway — both because a partial JSON document would fail to parse and
/// because reading the rest first would defeat the point of bounding memory
/// use against a misbehaving or hostile endpoint.
async fn read_bounded_body(response: reqwest::Response, limit: usize) -> Result<String, String> {
    let mut stream = response.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("failed to read response body: {error}"))?;
        if buf.len().saturating_add(chunk.len()) > limit {
            return Err("response too large".to_string());
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|error| format!("response body is not valid UTF-8: {error}"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, StatusConfig},
        upstream_status::Indicator,
    };

    fn config_with_source(refresh_seconds: u64, url: String) -> Config {
        let mut config = Config::default();
        config.server.status = Some(StatusConfig {
            refresh_seconds,
            sources: vec![StatusSource {
                provider: "claude".to_string(),
                url,
            }],
        });
        config
    }

    #[tokio::test]
    async fn spawn_status_poller_is_noop_without_status_config() {
        let state =
            AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();
        assert!(state.config.server.status.is_none());
        spawn_status_poller(state);
    }

    #[tokio::test]
    async fn spawn_status_poller_is_noop_with_empty_sources() {
        let mut config = Config::default();
        config.server.status = Some(StatusConfig {
            refresh_seconds: 300,
            sources: Vec::new(),
        });
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        spawn_status_poller(state);
    }

    #[tokio::test]
    async fn spawn_status_poller_is_noop_when_refresh_seconds_is_zero() {
        let config = config_with_source(0, "https://example.invalid/summary.json".to_string());
        assert!(config
            .server
            .status
            .as_ref()
            .unwrap()
            .refresh_interval()
            .is_none());
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        spawn_status_poller(state);
    }

    #[tokio::test]
    async fn refresh_seconds_below_the_floor_still_starts_the_poller() {
        // refresh_interval() clamps 30 -> 60; spawn_status_poller must treat
        // that as enabled, not disabled.
        let config = config_with_source(30, "https://example.invalid/summary.json".to_string());
        assert_eq!(
            config.server.status.as_ref().unwrap().refresh_interval(),
            Some(60)
        );
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        spawn_status_poller(state);
    }

    #[tokio::test]
    async fn poller_happy_path_fetches_and_applies_snapshot() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/summary.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": {"updated_at": "2026-08-12T14:25:14.330Z"},
                "incidents": [],
                "status": {"indicator": "none", "description": "All Systems Operational"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config = config_with_source(300, format!("{}/summary.json", server.uri()));
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        poll_all(&state).await;

        let snapshot = state.status.snapshot();
        let observed = snapshot.get("claude").expect("entry present");
        assert_eq!(observed.indicator, Indicator::None);
        assert_eq!(
            observed.description.as_deref(),
            Some("All Systems Operational")
        );
        assert!(observed.error.is_none());
    }

    #[tokio::test]
    async fn non_2xx_response_is_unknown_and_replaces_a_previous_good_value() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/summary.json"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let config = config_with_source(300, format!("{}/summary.json", server.uri()));
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        // Seed a previously-good value; the failed poll below must replace it
        // rather than leave it in place.
        state.status.set(
            "claude",
            UpstreamStatus {
                indicator: Indicator::None,
                description: Some("All Systems Operational".to_string()),
                incidents: Vec::new(),
                page_updated_at: None,
                observed_at: 1,
                error: None,
            },
        );

        poll_all(&state).await;

        let snapshot = state.status.snapshot();
        let observed = snapshot.get("claude").expect("entry present");
        assert_eq!(observed.indicator, Indicator::Unknown);
        assert_ne!(observed.indicator, Indicator::None);
        assert!(observed.error.is_some());
    }

    #[tokio::test]
    async fn oversized_body_is_unknown_with_a_too_large_error_not_a_partial_parse() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Pad well past MAX_BODY_BYTES with a value that would otherwise
        // parse fine, to prove the cap rejects on size rather than content.
        let oversized = format!(
            r#"{{"status":{{"indicator":"none"}},"padding":"{}"}}"#,
            "x".repeat(MAX_BODY_BYTES + 1024)
        );
        Mock::given(method("GET"))
            .and(path("/summary.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(oversized))
            .mount(&server)
            .await;

        let config = config_with_source(300, format!("{}/summary.json", server.uri()));
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        poll_all(&state).await;

        let snapshot = state.status.snapshot();
        let observed = snapshot.get("claude").expect("entry present");
        assert_eq!(observed.indicator, Indicator::Unknown);
        assert_eq!(observed.error.as_deref(), Some("response too large"));
    }

    #[tokio::test]
    async fn removed_sources_are_pruned_from_store_and_metrics() {
        let mut config = Config::default();
        config.server.status = Some(StatusConfig {
            refresh_seconds: 300,
            sources: Vec::new(),
        });
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        state.status.set(
            "removed",
            UpstreamStatus {
                indicator: Indicator::Major,
                description: None,
                incidents: Vec::new(),
                page_updated_at: None,
                observed_at: 1,
                error: None,
            },
        );
        metrics::record_upstream_status("removed", Some(2));

        poll_all(&state).await;

        assert!(!state.status.snapshot().contains_key("removed"));
        assert_eq!(metrics::upstream_status_value_for_tests("removed"), None);
    }

    #[test]
    fn non_2xx_html_error_body_yields_no_markup_and_no_newline() {
        // Reproduces a live capture: status.x.ai returns a 403 fronted by a
        // Cloudflare block page. That markup has no diagnostic value and,
        // rendered verbatim into the admin dashboard, made the table look
        // broken — the detail must drop it entirely rather than excerpt it.
        let cloudflare_page = "<!DOCTYPE html>\n<!--[if lt IE 7]> <html class=\"no-js ie6 oldie\" lang=\"en-US\"> <![endif]-->\n<title>Attention Required! | Cloudflare</title>";
        let detail = format_non_2xx_detail(reqwest::StatusCode::FORBIDDEN, cloudflare_page);
        assert_eq!(detail, "status request failed (403 Forbidden)");
        assert!(!detail.contains('<'));
        assert!(!detail.contains('\n'));
    }

    #[test]
    fn non_2xx_json_error_body_still_yields_its_excerpt() {
        let body = r#"{"error":{"code":"forbidden","message":"blocked by WAF"}}"#;
        let detail = format_non_2xx_detail(reqwest::StatusCode::FORBIDDEN, body);
        assert_eq!(
            detail,
            format!("status request failed (403 Forbidden): {body}")
        );
    }

    #[test]
    fn non_2xx_error_detail_collapses_whitespace_runs() {
        let body = "forbidden\n\tby   the   edge\nfirewall";
        let detail = format_non_2xx_detail(reqwest::StatusCode::FORBIDDEN, body);
        assert_eq!(
            detail,
            "status request failed (403 Forbidden): forbidden by the edge firewall"
        );
        assert!(!detail.contains('\n'));
        assert!(!detail.contains('\t'));
    }

    #[test]
    fn non_2xx_error_detail_caps_the_excerpt_at_120_chars() {
        let body = "x".repeat(500);
        let detail = format_non_2xx_detail(reqwest::StatusCode::FORBIDDEN, &body);
        let prefix = "status request failed (403 Forbidden): ";
        assert!(detail.starts_with(prefix));
        let excerpt = &detail[prefix.len()..];
        assert_eq!(excerpt.chars().count(), 120);
    }
}
