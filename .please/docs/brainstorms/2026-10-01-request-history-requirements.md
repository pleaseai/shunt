# Request history on the optional store — requirements

> Split out of track `pluggable-store-20261001` on 2026-10-01. That track builds the
> optional `[store]` (ADR-0006) and moves the three JSON snapshots onto it. This document
> holds the request-history half, to seed its own track with `/please:new-track`.
> It depends on the store landing first.

## Decided (carried over from the reviewed spec)

- Recording: 5-minute rollups keyed by provider, account, model, and status, holding
  request count, error count, latency sum and max, and token counts where the
  response reports them. Rows contain no request or response content and no
  inbound end-user or session identifier.
- Retention is configurable and defaults to 7 days. It is enforced by a periodic
  delete that touches history tables only.
- Durability class is write-behind. The proxy path never waits on the store. The
  pending-bucket backlog is bounded: when it is full, the oldest buckets are dropped
  and the drop is counted. Pending buckets are flushed on graceful shutdown.
- Model label policy (closes `dashboard-metrics.md` open question 5 for history only):
  - Record the id when it matches a configured `[[models]]` id, a
    `[[routes]]` model, a Codex inbound route, or the built-in catalog, after the
    same length bound and control-character sanitization that every stored label
    gets (ADR-0006), so a long or control-character-bearing configured label is
    never retained as-is.
  - Record every other id under one reserved overflow label.
  - Cap the number of distinct keys per open bucket. Past the cap, new keys go to
    the overflow label and the overflow is counted.
  - The OTel and Sentry label paths are unchanged.
- Exposure: an authenticated admin JSON read endpoint returns the buckets in a time
  range. With no store configured, it returns an explicit "history unavailable"
  result. Dashboard charts are a later track.
- Success: an operator can view at least 7 days of per-provider, per-account,
  per-model history spanning restarts.

## Acceptance criteria to carry (ids from the original spec)

AC-007 recording · AC-008 range read · AC-009 retention delete · AC-010 no content or
identity · AC-011 configured id verbatim · AC-032 overflow label · AC-033 per-bucket
key cap · AC-012 "unavailable" without a store · AC-015/AC-037 backlog drop and count ·
the history part of AC-024 (shutdown flush).

## What exploration found (2026-10-01) — the design problem this track must solve

- **Six recording sites, not one.** `record_proxied_request(provider, model, status,
  latency_ms)` (`src/metrics.rs:440`) is called from:
  - `src/proxy/failover/chain.rs:194`, per attempt; judge calls also arrive here
    with `caller: "router"`;
  - `src/codex_endpoint.rs:482`, inbound Codex HTTP and WS;
  - `src/proxy/chain_stream.rs:317` and `:804`, committed streams; status is
    hardcoded to 200;
  - `src/adapters/responses/early_stream.rs:436/465/476`;
  - `src/adapters/responses/pool.rs:285/296/500`.
- **The account is mostly absent at those sites.** It rides the `x-shunt-account`
  response header on non-committed responses, and an `event: account` SSE frame on
  pool early-commit streams. Only the Responses pool loop holds it as a variable.
  Non-pooled providers have no account at all, so they need a placeholder label.
- **Token counts exist only at stream end**, in `ObserverState::finish`
  (`src/stream_metrics.rs:422`). Non-streaming responses do no token accounting
  (`stream_metrics.rs:7`). A rollup therefore needs a second hook, or it has to
  accept "tokens: streaming only".
- **Samples are per attempt, not per request.** Failover advances and judge calls each
  record one sample. Decide whether history counts attempts, client requests, or both,
  for example keyed or filtered by `caller`.
- **No "is configured model" predicate exists.** `routing::resolve_chain` accepts
  arbitrary ids through the prefix and default arms. `BUILTIN_MODELS`
  (`src/discovery.rs:56`) is private and needs a `pub(crate)` lookup.
  `sanitize_model_tag` (`src/observability.rs:176`) only truncates.
- **Admin route inventory gates.** A new `/admin/api/*` route must be added in these
  places:
  - `tests/router_surface.rs` `ADMIN_PATHS`;
  - `tests/admin_surface.rs` (the absent, 401, and no-credential lists);
  - the path count in `docs/admin-ui-delivery.md:59`;
  - the endpoints reference page and its three locale copies.

## Open questions for the track

1. Should status be the HTTP code or an outcome class, and how is a mid-stream failure
   after a committed 200 recorded? `record_stream_outcome` carries no status or account.
2. Do rollups count attempts, client requests, or both?
3. Should the "tokens where available" gap on non-streaming paths be filled by parsing
   `usage` from the JSON body, or documented as a gap?
