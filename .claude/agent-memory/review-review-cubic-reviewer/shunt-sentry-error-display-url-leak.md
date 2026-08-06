---
name: shunt-sentry-error-display-url-leak
description: reqwest::Error Display appends `for url (...)` only when the error carries a URL — request-phase errors do, body/decode errors from bytes_stream() do not, so check the error kind before assuming an exported error chain leaks the host
metadata:
  type: project
---

`reqwest::Error`'s `Display` writes `" for url ({url})"` only when its inner `url` field is
`Some` (`reqwest-0.13.4/src/error.rs:279`). That field is populated by `.with_url(...)`, which
the request path applies (`async_impl/client.rs:3100`), so a request-phase error's
`to_string()` does carry the full upstream URL — host, path, and query.

**Why it matters:** wherever an error chain is exported as free text (Sentry extras, OTEL log
bodies, breadcrumbs), `sanitize_tag` in `src/observability.rs` only strips control characters
and truncates — it performs no URL or host redaction. An error that does carry a URL would
therefore reach Sentry with the host intact, against the "credentials, and the host name are
never sent" guarantee in `docs/running.md:139`.

**This does not apply to the #310 mid-stream path.** `src/stream_metrics.rs::error_chain`
renders a body-read error from `Response::bytes_stream()`, which reqwest builds through
`error::decode` — `Error::new(Kind::Decode, ...)` leaves `url: None`
(`reqwest-0.13.4/src/error.rs:41,331`), so there is no `for url (...)` clause to leak.
`record_stream_failure_event_carries_exactly_the_expected_extras` already asserts the exported
`upstream_error` contains no `://` (`src/observability.rs:1258`). An earlier revision of this
note asserted a confirmed doc/behavior contradiction on this path; that was wrong, and cubic
corrected it on PR #311.

**How to apply:** before concluding that an exported error chain does or does not leak a host,
check which reqwest error kind produced it — request, connect, and timeout errors carry a URL;
body and decode errors raised after a successful response do not. Redaction beyond
`sanitize_tag` is worth adding to any new export path that can surface request-phase errors.
