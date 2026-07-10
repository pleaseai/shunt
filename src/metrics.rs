//! Usage/performance metric emission.
//!
//! Metrics go to the operator's Sentry project and only when `[sentry]
//! metrics = true`; otherwise every capture below is dropped by the SDK (no
//! client bound, or `enable_metrics` off). Attributes stay low-cardinality
//! (provider/model/status) — never client names, session ids, or anything
//! request-derived.

use sentry::protocol::Unit;

/// Record one proxied inference request: a `shunt.requests` count and a
/// `shunt.latency` distribution, both tagged with provider, model (the
/// client-requested id), and the response status code.
pub fn record_proxied_request(provider: &str, model: &str, status: u16, latency_ms: f64) {
    sentry::metrics::counter("shunt.requests", 1)
        .attribute("provider", provider.to_owned())
        .attribute("model", model.to_owned())
        .attribute("http.response.status_code", i64::from(status))
        .capture();
    sentry::metrics::distribution("shunt.latency", latency_ms)
        .unit(Unit::Millisecond)
        .attribute("provider", provider.to_owned())
        .attribute("model", model.to_owned())
        .attribute("http.response.status_code", i64::from(status))
        .capture();
}

#[cfg(test)]
mod tests {
    use super::record_proxied_request;

    /// The core opt-in contract: with no Sentry client bound (the default),
    /// recording must be a silent no-op on every proxied request, never a
    /// panic.
    #[test]
    fn record_is_noop_without_sentry_client() {
        record_proxied_request("openai", "gpt-5.2", 200, 123.4);
        record_proxied_request("anthropic", "claude-opus-4-8", 502, 0.0);
    }
}
