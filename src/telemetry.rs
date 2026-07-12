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
//!   unaffected).
//!
//! The exporter uses reqwest's *blocking* client, so the SDK's batch span/log
//! processors and periodic metric reader run on their own dedicated threads,
//! fully independent of the axum serving runtime. That keeps init synchronous
//! (callable before the tokio runtime exists, like `init_sentry`) and lets the
//! [`TelemetryGuard`] flush on shutdown without a runtime.
//!
//! Privacy: exported telemetry stays low-cardinality and carries no
//! request/response bodies, headers, or credentials. The resource advertises
//! only `service.*`/`telemetry.sdk.*` (no host or process detector runs), and
//! the client session id rides on spans only when `include_session_id` is set.

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

    let (tracer, trace_layer) = if config.traces {
        let exporter = SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(signal_endpoint(&config.endpoint, "/v1/traces"))
            .with_headers(headers.clone())
            .build()?;
        // Parent-based so a client-supplied `traceparent` is honored; the ratio
        // governs root spans (shunt's request spans are roots unless the client
        // propagates a trace).
        let sampler =
            Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(config.sample_ratio)));
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
    } else {
        (None, None)
    };

    let meter = if config.metrics {
        let exporter = MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(signal_endpoint(&config.endpoint, "/v1/metrics"))
            .with_headers(headers.clone())
            .build()?;
        let provider = SdkMeterProvider::builder()
            .with_resource(resource.clone())
            .with_periodic_exporter(exporter)
            .build();
        // `crate::metrics` reads the global meter, so metric emission stays a
        // no-op until this provider is installed.
        opentelemetry::global::set_meter_provider(provider.clone());
        Some(provider)
    } else {
        None
    };

    let (logger, logs_layer) = if config.logs {
        let exporter = LogExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(signal_endpoint(&config.endpoint, "/v1/logs"))
            .with_headers(headers)
            .build()?;
        let provider = SdkLoggerProvider::builder()
            .with_resource(resource)
            .with_batch_exporter(exporter)
            .build();
        let layer = OpenTelemetryTracingBridge::new(&provider).boxed();
        (Some(provider), Some(layer))
    } else {
        (None, None)
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

/// The resource attributes on every exported signal. `service.name` is
/// overridable via config (or the standard `OTEL_SERVICE_NAME` env var picked
/// up by the SDK's env detector); `service.version` is shunt's build version;
/// `deployment.environment.name` is set only when configured.
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
