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
    let (tracer, trace_layer) = match span_exporter {
        Some(exporter) => {
            // Parent-based so a client-supplied `traceparent` is honored; the
            // ratio governs root spans (shunt's request spans are roots unless
            // the client propagates a trace). Clamp defensively so a caller that
            // built `OtelConfig` without `Config::validate()` can't hand an
            // out-of-range probability to the sampler.
            let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                config.sample_ratio.clamp(0.0, 1.0),
            )));
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
    use super::signal_endpoint;

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
}
