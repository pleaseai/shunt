//! OpenTelemetry (OTLP) export pipeline.
//!
//! Opt-in via `[otel]`: absent config means this module never runs and nothing
//! leaves the machine. When enabled it exports up to three signals over
//! OTLP/HTTP (protobuf) to the operator's own endpoint (an OpenTelemetry
//! Collector or compatible backend):
//!
//! - **traces** — the per-request `proxy_request` span, sampled head-based.
//! - **metrics** — the `shunt.requests`/`shunt.latency` series (see
//!   [`crate::metrics`]), mirroring the Sentry metrics.
//! - **logs** — `tracing` events bridged to OTLP logs (stderr `fmt` logs are
//!   unaffected). Unlike metrics/traces this mirrors shunt's diagnostic log
//!   events verbatim, so it can carry request-derived fields (e.g. an upstream
//!   error body, an authenticated client id) — see the privacy note below.
//!
//! The exporter uses reqwest's *blocking* client, so the SDK's batch span/log
//! processors and periodic metric reader run on their own dedicated threads,
//! fully independent of the axum serving runtime. That keeps init synchronous
//! (callable before the tokio runtime exists, like `init_sentry`) and lets the
//! [`TelemetryGuard`] flush on shutdown without a runtime.
//!
//! Privacy: **metrics** and **traces** stay low-cardinality and carry no
//! request/response bodies, headers, or credentials — the request span's client
//! session id is attached only when `include_session_id` is set (see
//! [`crate::proxy`]). **Logs**, however, export shunt's own diagnostic events as
//! written, so they can include request-derived fields the same way the stderr
//! logs do; an operator wanting strictly body-free export can set `logs = false`
//! and keep metrics/traces. All signals go only to the operator-configured OTLP
//! endpoint. The resource advertises `service.*`/`telemetry.sdk.*` (no host or
//! process detector runs) plus whatever the operator puts in the standard
//! `OTEL_RESOURCE_ATTRIBUTES` env var.

use std::collections::HashMap;
use std::sync::OnceLock;

use opentelemetry::{trace::TracerProvider as _, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{
    LogExporter, MetricExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig,
};
use opentelemetry_sdk::{
    logs::SdkLoggerProvider,
    metrics::SdkMeterProvider,
    trace::{Sampler, SdkTracerProvider},
    Resource,
};
use opentelemetry_semantic_conventions::attribute::{DEPLOYMENT_ENVIRONMENT_NAME, SERVICE_VERSION};
use tracing_subscriber::{Layer, Registry};

use crate::config::OtelConfig;

/// Instrumentation scope name for shunt-emitted telemetry.
pub const SCOPE: &str = "shunt";

/// Whether the client `session_id` must be withheld from request trace spans,
/// pinned once at startup by [`init`]. Read via [`withhold_session_id`].
static WITHHOLD_SESSION_ID: OnceLock<bool> = OnceLock::new();

/// Whether the per-request client `session_id` must be kept off trace spans.
///
/// The decision is fixed for the process lifetime because the OTLP exporter is
/// built exactly once at startup and never rebuilt on hot-reload — so the
/// privacy rule that governs what that exporter emits must be pinned to the
/// startup `[otel]` config, not read from the hot-swappable per-request config
/// (which a mid-run edit could flip while the original exporter keeps running).
///
/// `true` only when the *trace bridge* is active (the sole signal that carries
/// span fields to the collector) and the operator did not opt in via
/// `include_session_id`. Stays `false` when `[otel]` is absent/disabled (`init`
/// never ran) or exports only metrics/logs, so local stderr spans keep the id.
pub fn withhold_session_id() -> bool {
    WITHHOLD_SESSION_ID.get().copied().unwrap_or(false)
}

/// The startup withhold decision (see [`withhold_session_id`]). Withhold only
/// when the trace bridge will actually export span fields (`traces`) and the
/// operator has not opted in — a metrics/logs-only export leaves the id on the
/// local span. Pure so it is unit-testable without touching the global.
fn should_withhold_session_id(config: &OtelConfig) -> bool {
    config.traces && !config.include_session_id
}

/// The subscriber-layer slot the OTel trace + logs bridges are injected into
/// after config load. `None` until (and unless) `[otel]` is enabled; the
/// `reload` layer holding it is installed on `Registry` at process start.
pub type OtelReloadLayer = Option<Box<dyn Layer<Registry> + Send + Sync>>;

/// Keeps the SDK providers alive and flushes/shuts them down on drop, so
/// buffered telemetry is exported before the process exits. Must outlive the
/// tokio runtime (like the Sentry guard); the exporters run on their own
/// threads and do not depend on it.
pub struct TelemetryGuard {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // Flush and stop the exporter threads. Warnings go to the stderr `fmt`
        // layer regardless of the OTLP logger's state.
        if let Some(provider) = &self.tracer {
            if let Err(error) = provider.shutdown() {
                tracing::warn!(%error, "otel tracer provider shutdown failed");
            }
        }
        if let Some(provider) = &self.meter {
            if let Err(error) = provider.shutdown() {
                tracing::warn!(%error, "otel meter provider shutdown failed");
            }
        }
        if let Some(provider) = &self.logger {
            if let Err(error) = provider.shutdown() {
                tracing::warn!(%error, "otel logger provider shutdown failed");
            }
        }
    }
}

/// Build the OTLP pipeline from an enabled `[otel]` config: install the global
/// meter/tracer providers and return the guard plus the subscriber layer to
/// inject via the `reload` handle. Callers must check [`OtelConfig::enabled`]
/// first — a disabled section never reaches here.
///
/// Errors only on exporter construction (e.g. an unreachable endpoint is *not*
/// an error here — export failures are async and logged by the SDK).
pub fn init(config: &OtelConfig) -> anyhow::Result<(TelemetryGuard, OtelReloadLayer)> {
    let resource = build_resource(config);
    let headers = header_map(config);

    // Build every enabled exporter FIRST. Exporter construction is the only
    // fallible step, and none of the `opentelemetry::global::set_*` calls below
    // run until all three have succeeded — so a failure on any one signal
    // returns `Err` with no global provider installed and no background export
    // thread leaked (a partial install would otherwise keep exporting, e.g.
    // metrics, despite the caller logging "failed to initialize").
    let span_exporter = config
        .traces
        .then(|| {
            configure(
                SpanExporter::builder().with_http(),
                &config.endpoint,
                "/v1/traces",
                &headers,
            )
            .build()
        })
        .transpose()?;
    let metric_exporter = config
        .metrics
        .then(|| {
            configure(
                MetricExporter::builder().with_http(),
                &config.endpoint,
                "/v1/metrics",
                &headers,
            )
            .build()
        })
        .transpose()?;
    let log_exporter = config
        .logs
        .then(|| {
            configure(
                LogExporter::builder().with_http(),
                &config.endpoint,
                "/v1/logs",
                &headers,
            )
            .build()
        })
        .transpose()?;

    // All exporters constructed — now install providers/globals and assemble
    // the subscriber layers. Nothing below can fail.

    // Pin the session-id privacy decision for the process lifetime (see
    // `withhold_session_id`), only now that construction has succeeded: a
    // fallible exporter build above returns `Err` and `init_telemetry` then runs
    // without any export, so pinning earlier would leave the flag `true` and
    // silently strip `session_id` from local stderr spans with nothing exported.
    // On the success path this config governs the exporter for its whole life
    // even if `[otel]` is later hot-edited — the exporter is never rebuilt.
    // `set` is idempotent (first call wins).
    let _ = WITHHOLD_SESSION_ID.set(should_withhold_session_id(config));

    let (tracer, trace_layer) = match span_exporter {
        Some(exporter) => {
            // Parent-based: a span that already carries a parent context
            // inherits its parent's sampling decision; otherwise the ratio
            // governs the (root) request span. shunt does not yet extract an
            // inbound W3C `traceparent` from request headers, so today every
            // request span is a root — wiring inbound trace-context propagation
            // is tracked as a follow-up. Clamp defensively so a caller that
            // built `OtelConfig` without `Config::validate()` (which already
            // rejects out-of-range and NaN ratios) can't hand a bad probability
            // to the sampler; map NaN — which `clamp` would pass through — to
            // full sampling rather than feed the sampler an undefined ratio.
            let ratio = if config.sample_ratio.is_nan() {
                1.0
            } else {
                config.sample_ratio.clamp(0.0, 1.0)
            };
            let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)));
            let provider = SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .with_sampler(sampler)
                .with_batch_exporter(exporter)
                .build();
            let layer = tracing_opentelemetry::layer()
                .with_tracer(provider.tracer(SCOPE))
                .boxed();
            opentelemetry::global::set_tracer_provider(provider.clone());
            (Some(provider), Some(layer))
        }
        None => (None, None),
    };

    let meter = match metric_exporter {
        Some(exporter) => {
            let provider = SdkMeterProvider::builder()
                .with_resource(resource.clone())
                .with_periodic_exporter(exporter)
                .build();
            // `crate::metrics` reads the global meter, so metric emission stays
            // a no-op until this provider is installed.
            opentelemetry::global::set_meter_provider(provider.clone());
            Some(provider)
        }
        None => None,
    };

    let (logger, logs_layer) = match log_exporter {
        Some(exporter) => {
            let provider = SdkLoggerProvider::builder()
                .with_resource(resource)
                .with_batch_exporter(exporter)
                .build();
            let layer = OpenTelemetryTracingBridge::new(&provider).boxed();
            (Some(provider), Some(layer))
        }
        None => (None, None),
    };

    // A `Vec<Box<dyn Layer>>` is itself a `Layer`, so the enabled bridges
    // compose into the single slot the reload handle swaps in.
    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();
    layers.extend(trace_layer);
    layers.extend(logs_layer);
    let layer: OtelReloadLayer = if layers.is_empty() {
        None
    } else {
        Some(Box::new(layers))
    };

    Ok((
        TelemetryGuard {
            tracer,
            meter,
            logger,
        },
        layer,
    ))
}

/// Apply the shared OTLP/HTTP settings (protobuf, base endpoint + signal path,
/// headers) to a signal's exporter builder. The caller calls the concrete
/// `.build()` (not a trait method) on the returned builder.
fn configure<B>(builder: B, base: &str, path: &str, headers: &HashMap<String, String>) -> B
where
    B: WithExportConfig + WithHttpConfig,
{
    builder
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(signal_endpoint(base, path))
        .with_headers(headers.clone())
}

/// The resource attributes on every exported signal. `service.name` comes from
/// config (default "shunt"); because shunt always sets it explicitly it takes
/// precedence over the SDK env detector, so `OTEL_SERVICE_NAME` does not
/// override it — set `[otel] service_name` (or `SHUNT_OTEL__SERVICE_NAME`)
/// instead. `service.version` is shunt's build version; `deployment.environment
/// .name` is set only when configured. `OTEL_RESOURCE_ATTRIBUTES` is still
/// merged in by the SDK's env detector for any extra attributes.
fn build_resource(config: &OtelConfig) -> Resource {
    let mut builder = Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attribute(KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")));
    if let Some(environment) = &config.environment {
        builder = builder.with_attribute(KeyValue::new(
            DEPLOYMENT_ENVIRONMENT_NAME,
            environment.clone(),
        ));
    }
    builder.build()
}

/// OTLP request headers (e.g. a hosted-collector auth token). The SDK also
/// merges `OTEL_EXPORTER_OTLP_HEADERS` from the env on top of these.
fn header_map(config: &OtelConfig) -> HashMap<String, String> {
    config.headers.clone().into_iter().collect()
}

/// Join the configured base endpoint with a signal path. Programmatic
/// `.with_endpoint()` is used verbatim by the exporter (unlike the env-var base
/// endpoint, which auto-appends the signal path), so shunt appends it here to
/// keep `endpoint` a base URL like `OTEL_EXPORTER_OTLP_ENDPOINT`.
fn signal_endpoint(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        build_resource, header_map, init, should_withhold_session_id, signal_endpoint,
        withhold_session_id,
    };
    use crate::config::OtelConfig;

    /// A minimal enabled `[otel]` config pointing at a loopback port with nothing
    /// listening. `init` only *constructs* exporters (it never connects), so the
    /// unreachable endpoint is irrelevant here — no telemetry is emitted in these
    /// tests, so shutdown flushes nothing.
    fn config(traces: bool, metrics: bool, logs: bool, environment: Option<&str>) -> OtelConfig {
        OtelConfig {
            endpoint: "http://127.0.0.1:4318".to_string(),
            service_name: "shunt-test".to_string(),
            environment: environment.map(ToOwned::to_owned),
            sample_ratio: 0.5,
            headers: BTreeMap::new(),
            traces,
            metrics,
            logs,
            include_session_id: false,
        }
    }

    #[test]
    fn signal_endpoint_appends_path() {
        assert_eq!(
            signal_endpoint("http://localhost:4318", "/v1/traces"),
            "http://localhost:4318/v1/traces"
        );
    }

    #[test]
    fn signal_endpoint_collapses_trailing_slash() {
        assert_eq!(
            signal_endpoint("http://localhost:4318/", "/v1/metrics"),
            "http://localhost:4318/v1/metrics"
        );
    }

    #[test]
    fn header_map_carries_configured_headers() {
        let mut cfg = config(true, true, true, None);
        cfg.headers
            .insert("authorization".to_string(), "Bearer token".to_string());
        let headers = header_map(&cfg);
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer token")
        );
    }

    #[test]
    fn build_resource_succeeds_with_and_without_environment() {
        // Exercises both the environment `Some` (attribute added) and `None`
        // (attribute skipped) arms of `build_resource`.
        let _with_env = build_resource(&config(true, true, true, Some("prod")));
        let _without_env = build_resource(&config(true, true, true, None));
    }

    #[test]
    fn init_metrics_only_installs_provider_without_layer() {
        // Only metrics enabled: the meter provider is installed, but there is no
        // trace or logs subscriber bridge, so the reload layer stays `None`.
        let (guard, layer) = init(&config(false, true, false, None)).expect("init should succeed");
        assert!(
            layer.is_none(),
            "no trace/logs bridge means no subscriber layer to inject"
        );
        drop(guard); // exercises the meter-provider shutdown path
    }

    #[test]
    fn init_traces_and_logs_builds_subscriber_layer() {
        // Traces + logs enabled (metrics off): both the trace and logs bridges
        // are assembled into the single reload layer, and the guard shuts down
        // the tracer and logger providers on drop.
        let (guard, layer) =
            init(&config(true, false, true, Some("staging"))).expect("init should succeed");
        assert!(
            layer.is_some(),
            "trace + logs bridges must compose into a subscriber layer"
        );
        drop(guard); // exercises the tracer/logger shutdown paths
    }

    #[test]
    fn init_tolerates_nan_sample_ratio() {
        // A caller that skipped `Config::validate()` could pass a NaN ratio;
        // the sampler must be built without panicking (NaN maps to full sampling).
        let mut cfg = config(true, false, false, None);
        cfg.sample_ratio = f64::NAN;
        let (guard, _layer) = init(&cfg).expect("init should tolerate a NaN ratio");
        drop(guard);
    }

    #[test]
    fn should_withhold_session_id_only_when_trace_bridge_active_and_not_opted_in() {
        // Trace export on, no opt-in → withhold (the id would ride the span).
        assert!(should_withhold_session_id(&config(true, true, true, None)));
        // Opted in → keep, even with traces on.
        let mut opted_in = config(true, true, true, None);
        opted_in.include_session_id = true;
        assert!(!should_withhold_session_id(&opted_in));
        // Metrics/logs only (no trace bridge) → keep the id on local spans.
        assert!(!should_withhold_session_id(&config(
            false, true, true, None
        )));
        // Exercise the public getter (its value reflects process-global startup
        // state, so only assert that the call itself is well-formed).
        let _: bool = withhold_session_id();
    }
}
