//! Inbound OTLP/HTTP telemetry ingest for the Claude apps gateway (M-C, #189).
//!
//! Not to be confused with [`crate::telemetry`]: that module is shunt's own
//! **outbound** export of the metrics and traces shunt itself produces, driven
//! by `[otel]`. This module is the **inbound** direction — it accepts the OTLP
//! payloads managed Claude Code clients export to the gateway, because M-B's
//! managed settings point those clients here by setting
//! `OTEL_EXPORTER_OTLP_ENDPOINT` to `[server.gateway].public_url`.
//!
//! Payloads are relayed **verbatim**: the exact request bytes are POSTed to
//! each opted-in destination without parsing or re-encoding. Claude Code stamps
//! the user, session, and organization attribution attributes client-side, so
//! decoding and re-serializing would only risk dropping fields shunt does not
//! model — and it lets the same handler serve both `application/x-protobuf` and
//! `application/json` exporters.
//!
//! The accept path always answers `200` with `{}`. Relays are detached tasks,
//! so a slow or unreachable collector never becomes client-visible latency, and
//! a signal with no opted-in destination is accepted and discarded.

use std::time::Duration;

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;

use crate::{config::GatewayTelemetryDestination, error::ShuntError, server::AppState};

/// Inbound body cap. Matches the reference Claude apps gateway's default
/// inbound `limits.max_request_bytes` of 32 MiB. An over-cap body is rejected
/// rather than truncated: a partial OTLP payload is not a valid one.
const MAX_TELEMETRY_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Ceiling on one relay attempt. Applied per request rather than on the client:
/// the shared [`AppState::http_client`] is deliberately timeout-free because it
/// also carries streaming inference, which may legitimately run for minutes.
/// Without a bound here, a destination that accepts the connection and then
/// never answers would pin a detached task and its payload bytes indefinitely,
/// one per client flush.
const RELAY_TIMEOUT: Duration = Duration::from_secs(30);

/// The three OTLP/HTTP signals the gateway ingests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Signal {
    Metrics,
    Logs,
    Traces,
}

impl Signal {
    /// OTLP/HTTP path for this signal, both the route shunt serves and the
    /// suffix it appends to a destination's base endpoint.
    pub(crate) const fn path(self) -> &'static str {
        match self {
            Self::Metrics => "/v1/metrics",
            Self::Logs => "/v1/logs",
            Self::Traces => "/v1/traces",
        }
    }

    /// Low-cardinality label for logs and metrics.
    const fn label(self) -> &'static str {
        match self {
            Self::Metrics => "metrics",
            Self::Logs => "logs",
            Self::Traces => "traces",
        }
    }

    /// Whether this destination opted in to receiving this signal.
    fn opted_in(self, destination: &GatewayTelemetryDestination) -> bool {
        match self {
            Self::Metrics => destination.metrics,
            Self::Logs => destination.logs,
            Self::Traces => destination.traces,
        }
    }
}

pub async fn metrics(state: State<AppState>, headers: HeaderMap, body: Body) -> Response {
    ingest(Signal::Metrics, state, headers, body).await
}

pub async fn logs(state: State<AppState>, headers: HeaderMap, body: Body) -> Response {
    ingest(Signal::Logs, state, headers, body).await
}

pub async fn traces(state: State<AppState>, headers: HeaderMap, body: Body) -> Response {
    ingest(Signal::Traces, state, headers, body).await
}

/// Accept one OTLP payload and fan it out to the destinations that opted in to
/// this signal, without waiting for any of them.
async fn ingest(
    signal: Signal,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let state = state.refreshed();
    // Same guard as `managed::get`: a reload that removed `[server.gateway]`
    // leaves the boot-registered routes in place with no auth to check against,
    // so fail closed rather than accept unauthenticated telemetry.
    let Some(auth) = &state.gateway_auth else {
        return unauthorized(signal);
    };
    if auth.authenticate_bearer(&headers).is_none() {
        return unauthorized(signal);
    }

    // `to_bytes` fails on an over-cap body and on a mid-body transport error.
    // Both are reported as `413`: the size limit is the only cause a client can
    // act on, and a dropped upload has no client waiting for a better answer.
    let Ok(body) = to_bytes(body, MAX_TELEMETRY_BODY_BYTES).await else {
        crate::metrics::record_gateway_telemetry_ingest(signal.label(), "rejected");
        return ShuntError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            format!(
                "OTLP {} payload could not be read within the {} MiB inbound limit",
                signal.label(),
                MAX_TELEMETRY_BODY_BYTES / (1024 * 1024)
            ),
        )
        .into_response();
    };

    let destinations = state
        .config
        .server
        .gateway
        .as_ref()
        .and_then(|gateway| gateway.telemetry.as_ref())
        .map(|telemetry| telemetry.forward_to.as_slice())
        .unwrap_or_default();

    // Only the payload framing headers cross over. The client's `authorization`
    // is a gateway JWT that means nothing to a collector, and forwarding any
    // other inbound header would let a client reach a destination's own auth.
    let content_type = headers.get(header::CONTENT_TYPE).cloned();
    let content_encoding = headers.get(header::CONTENT_ENCODING).cloned();

    let mut relayed = 0usize;
    for destination in destinations
        .iter()
        .filter(|destination| signal.opted_in(destination))
    {
        spawn_relay(
            state.http_client.clone(),
            destination.clone(),
            signal,
            // Cloning `Bytes` bumps a refcount; the payload itself is shared.
            body.clone(),
            content_type.clone(),
            content_encoding.clone(),
        );
        relayed += 1;
    }

    if relayed == 0 {
        tracing::debug!(
            signal = signal.label(),
            bytes = body.len(),
            "discarded inbound gateway telemetry: no destination opted in to this signal"
        );
    }
    crate::metrics::record_gateway_telemetry_ingest(
        signal.label(),
        if relayed == 0 { "discarded" } else { "relayed" },
    );

    (StatusCode::OK, Json(serde_json::json!({}))).into_response()
}

fn unauthorized(signal: Signal) -> Response {
    crate::metrics::record_gateway_telemetry_ingest(signal.label(), "rejected");
    ShuntError::new(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "missing or invalid gateway bearer token",
    )
    .into_response()
}

/// Relay one payload to one destination as a detached task, so the client's
/// `200` never waits on a collector and destinations proceed concurrently.
fn spawn_relay(
    client: reqwest::Client,
    destination: GatewayTelemetryDestination,
    signal: Signal,
    body: Bytes,
    content_type: Option<HeaderValue>,
    content_encoding: Option<HeaderValue>,
) {
    tokio::spawn(async move {
        let url = signal_url(&destination.url, signal);
        let mut request = client.post(&url).timeout(RELAY_TIMEOUT).body(body);
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(content_encoding) = content_encoding {
            request = request.header(header::CONTENT_ENCODING, content_encoding);
        }
        // Destination headers are applied last so an operator-configured value
        // (a collector API key, say) is authoritative for its key.
        for (name, value) in destination.headers.iter().flatten() {
            request = request.header(name, value);
        }

        // Failures are logged with the destination host and signal only —
        // never a header value and never any part of the payload, which
        // carries client prompts and file paths.
        let host = destination_host(&url);
        match request.send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => tracing::warn!(
                signal = signal.label(),
                destination = host,
                status = response.status().as_u16(),
                "gateway telemetry relay rejected by destination"
            ),
            Err(error) => tracing::warn!(
                signal = signal.label(),
                destination = host,
                %error,
                "gateway telemetry relay failed"
            ),
        }
    });
}

/// The destination's base OTLP endpoint plus this signal's path, mirroring the
/// `OTEL_EXPORTER_OTLP_ENDPOINT` convention the configured URL follows.
fn signal_url(base: &str, signal: Signal) -> String {
    format!("{}{}", base.trim().trim_end_matches('/'), signal.path())
}

/// Host for log lines. Config validation requires an http(s) URL with a host,
/// so the fallback only covers a URL that stopped parsing after path joining.
fn destination_host(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::{signal_url, Signal};
    use crate::config::GatewayTelemetryDestination;

    fn destination(metrics: bool, logs: bool, traces: bool) -> GatewayTelemetryDestination {
        GatewayTelemetryDestination {
            url: "https://collector.example".to_string(),
            headers: None,
            metrics,
            logs,
            traces,
        }
    }

    #[test]
    fn signal_url_appends_the_signal_path_to_a_base_endpoint() {
        assert_eq!(
            signal_url("https://collector.example", Signal::Metrics),
            "https://collector.example/v1/metrics"
        );
        assert_eq!(
            signal_url("https://collector.example/", Signal::Logs),
            "https://collector.example/v1/logs"
        );
        // A base with a path prefix keeps it; only a trailing slash is trimmed.
        assert_eq!(
            signal_url("  https://collector.example/otlp/  ", Signal::Traces),
            "https://collector.example/otlp/v1/traces"
        );
    }

    #[test]
    fn opted_in_reads_the_per_signal_flag_of_its_own_signal() {
        let metrics_only = destination(true, false, false);
        assert!(Signal::Metrics.opted_in(&metrics_only));
        assert!(!Signal::Logs.opted_in(&metrics_only));
        assert!(!Signal::Traces.opted_in(&metrics_only));

        let traces_only = destination(false, false, true);
        assert!(!Signal::Metrics.opted_in(&traces_only));
        assert!(Signal::Traces.opted_in(&traces_only));
    }
}
