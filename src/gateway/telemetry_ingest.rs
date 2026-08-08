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
//! the `user.id`, `user.email`, and `user.groups` attribution attributes
//! client-side, from the gateway-issued JWT, so decoding and re-serializing
//! would only risk dropping fields shunt does not model — and it lets the same
//! handler serve both `application/x-protobuf` and `application/json`
//! exporters.
//!
//! The accept path always answers `200` with `{}`. Relays are detached tasks,
//! so a slow or unreachable collector never becomes client-visible latency, and
//! a signal with no opted-in destination is accepted and discarded.

use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;

use crate::{config::GatewayTelemetryDestination, error::ShuntError, server::AppState};

/// Inbound body cap. Matches the default inbound `limits.max_request_bytes` of
/// 32 MiB documented in the Claude apps gateway configuration reference's HTTP
/// tuning table (<https://code.claude.com/docs/en/claude-apps-gateway-config>).
/// An over-cap body is rejected rather than truncated: a partial OTLP payload
/// is not a valid one.
const MAX_TELEMETRY_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Ceiling on one relay attempt. Without it, a destination that accepts the
/// connection and then never answers would pin a detached task and its payload
/// bytes indefinitely, one per client flush. Applied per request so the bound
/// is visible next to the other relay settings; [`relay_client`] carries only
/// the redirect policy.
const RELAY_TIMEOUT: Duration = Duration::from_secs(30);

/// How many relay tasks may be in flight at once, across all destinations and
/// signals, per [`crate::gateway::GatewayStores`].
///
/// Relays are detached, so their payload bytes outlive the request that
/// accepted them: the inbound concurrency permit rides the response body and
/// releases as soon as the empty `{}` is written, while a relay may hold its
/// copy for up to [`RELAY_TIMEOUT`]. Without a bound, retained heap would be
/// "everything accepted in the last 30 seconds", which is set by client
/// traffic rather than by anything shunt controls.
///
/// The arithmetic: worst-case resident payload bytes are
/// `MAX_INFLIGHT_RELAYS × MAX_TELEMETRY_BODY_BYTES` = 64 × 32 MiB = 2 GiB, and
/// that requires every one of the 64 slots to hold a maximum-size body at the
/// same moment. Real OTLP exports run orders of magnitude smaller, so the
/// practical ceiling is far below that; the number's job is to make the worst
/// case finite and computable rather than open-ended.
pub(crate) const MAX_INFLIGHT_RELAYS: usize = 64;

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
    // Defensive, not a reload path: `reload` deliberately keeps the previous
    // `gateway_auth` when a new config drops `[server.gateway]`, warning that
    // toggling the table needs a restart, so JWTs issued before such a reload
    // keep authenticating until then. `None` therefore means the state was
    // built without a gateway at all, and there is nothing to authenticate
    // against — fail closed.
    let Some(auth) = &state.gateway_auth else {
        return unauthorized(signal);
    };
    if auth.authenticate_bearer(&headers).is_none() {
        return unauthorized(signal);
    }

    // `to_bytes` fails on an over-cap body and on a mid-body transport error.
    // Both are reported as `413` `request_too_large` — the gateway-wide type
    // for a body over a cap (docs/gateway-protocol.md): the size limit is the
    // only cause a client can act on, and a dropped upload has no client
    // waiting for a better answer.
    let Ok(body) = to_bytes(body, MAX_TELEMETRY_BODY_BYTES).await else {
        crate::metrics::record_gateway_telemetry_ingest(signal.label(), "rejected");
        return ShuntError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
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
    let mut shed = 0usize;
    for destination in destinations
        .iter()
        .filter(|destination| signal.opted_in(destination))
    {
        // `try_acquire_owned`, never `acquire`: waiting for admission would put
        // the saturated case back on the client's critical path, which is the
        // one thing the detached design exists to avoid. A saturated gateway
        // drops this payload instead — telemetry is lossy by nature, and
        // shedding it is strictly better than growing unbounded heap.
        let Ok(permit) = state
            .gateway_stores
            .telemetry_relay_permits
            .clone()
            .try_acquire_owned()
        else {
            shed += 1;
            continue;
        };
        spawn_relay(
            destination.clone(),
            signal,
            // Cloning `Bytes` bumps a refcount; the payload itself is shared.
            body.clone(),
            content_type.clone(),
            content_encoding.clone(),
            permit,
        );
        relayed += 1;
    }

    if shed > 0 {
        tracing::warn!(
            signal = signal.label(),
            shed,
            relayed,
            limit = MAX_INFLIGHT_RELAYS,
            "shed inbound gateway telemetry relays at the in-flight limit"
        );
    } else if relayed == 0 {
        tracing::debug!(
            signal = signal.label(),
            bytes = body.len(),
            "discarded inbound gateway telemetry: no destination opted in to this signal"
        );
    }
    // One count per request. `shed` wins over `discarded` when nothing was
    // relayed, so saturation is never reported as a routine discard.
    crate::metrics::record_gateway_telemetry_ingest(
        signal.label(),
        match (relayed, shed) {
            (0, 0) => "discarded",
            (0, _) => "shed",
            _ => "relayed",
        },
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
///
/// `permit` is moved into the task and dropped when it ends, so a slot stays
/// consumed for exactly as long as the payload bytes are resident.
fn spawn_relay(
    destination: GatewayTelemetryDestination,
    signal: Signal,
    body: Bytes,
    content_type: Option<HeaderValue>,
    content_encoding: Option<HeaderValue>,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    tokio::spawn(async move {
        let _permit = permit;
        let url = signal_url(&destination.url, signal);
        let host = destination_host(&url);
        let mut request = relay_client().post(&url).timeout(RELAY_TIMEOUT).body(body);
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(content_encoding) = content_encoding {
            request = request.header(header::CONTENT_ENCODING, content_encoding);
        }
        // Destination headers are applied last, and as a map rather than
        // key-by-key: `RequestBuilder::header` *appends*, which would emit two
        // `content-type` headers when an operator configures one, while
        // `RequestBuilder::headers` routes through `replace_headers`, which
        // inserts per key. So an operator-configured value (a collector API
        // key, or a deliberate `content-type` override) really is authoritative
        // for its key.
        request = request.headers(destination_headers(&destination, &host));

        // Failures are logged with the destination host and signal only — never
        // a header value and never any part of the payload, which carries
        // client prompts and file paths. `without_url` is load-bearing here:
        // reqwest's `Display` appends " for url (…)", which would put the full
        // relay URL, including any userinfo or query secrets, into the log.
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
                error = %error.without_url(),
                "gateway telemetry relay failed"
            ),
        }
    });
}

/// The HTTP client relays use. Separate from the shared [`AppState`] client so
/// it can refuse redirects: reqwest's default policy follows up to 10 hops and
/// strips only `Authorization`/`Cookie`-class headers on a cross-host redirect,
/// so a destination's configured `x-api-key` would follow a 3xx to whatever
/// host it names. An OTLP collector has no legitimate reason to redirect, so a
/// 3xx instead surfaces through the non-success branch above. Sibling code
/// hardens its redirect policy the same way (`gateway::store`, `auth::shared`).
fn relay_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build redirect-hardened telemetry relay client")
    })
}

/// The destination's configured headers as a [`HeaderMap`].
///
/// Config validation is the real gate: it rejects an invalid header name or
/// value at boot and on every reload, so a destination that reached here
/// through `[server.gateway.telemetry]` has already been checked. This skip is
/// defense-in-depth for a destination constructed directly in code, and it
/// matters because with `RequestBuilder::header` an invalid name poisoned the
/// whole builder and silently dropped the payload. Each distinct offender is
/// warned about once per process; the value is never logged.
fn destination_headers(destination: &GatewayTelemetryDestination, host: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in destination.headers.iter().flatten() {
        match (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            (Ok(name), Ok(value)) => {
                headers.insert(name, value);
            }
            _ => warn_invalid_header_once(host, name),
        }
    }
    headers
}

fn warn_invalid_header_once(host: &str, name: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.insert(format!("{host}\u{0}{name}")) {
        tracing::warn!(
            destination = host,
            header = name,
            "skipping invalid gateway telemetry destination header"
        );
    }
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

    /// Defense-in-depth for a destination built directly in code: config
    /// validation rejects an invalid header before it can reach here, but if
    /// one does, it is skipped and its well-formed siblings still apply. With
    /// `RequestBuilder::header` an invalid name poisoned the builder and the
    /// whole payload was dropped.
    #[test]
    fn invalid_destination_headers_are_skipped_without_dropping_valid_ones() {
        let mut destination = destination(true, false, false);
        destination.headers = Some(
            [
                (
                    "x-collector-key".to_string(),
                    "collector-secret".to_string(),
                ),
                // A space is not legal in an HTTP header name.
                ("x collector key".to_string(), "ignored".to_string()),
                // A newline is not legal in a header value.
                ("x-tenant".to_string(), "line\nbreak".to_string()),
            ]
            .into_iter()
            .collect(),
        );

        let headers = super::destination_headers(&destination, "collector.example");
        assert_eq!(headers.get("x-collector-key").unwrap(), "collector-secret");
        assert!(headers.get("x collector key").is_none());
        assert!(headers.get("x-tenant").is_none());
        // Only the well-formed header survived, so the relay still fires with a
        // valid map rather than a poisoned builder.
        assert_eq!(headers.len(), 1);
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
