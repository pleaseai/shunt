---
title: Changelog
description: Every notable change to shunt, dated, with breaking changes flagged.
---

Curated release notes, newest first. shunt is pre-1.0, so a minor bump (`0.44` → `0.45`)
may carry breaking changes and a patch bump does not. Every breaking entry below names who
it affects, what to do, and the release it landed in.

- **Subscribe:** [GitHub Releases feed](https://github.com/pleaseai/shunt/releases.atom)
- **Complete record:** [`CHANGELOG.md`](https://github.com/pleaseai/shunt/blob/main/CHANGELOG.md), generated from every commit
- **Upgrading:** [Installation](/getting-started/installation/)

This page covers 0.35.0 onward. Releases before that are in the generated changelog only.

## 0.45.1 — 2026-09-14

### Native tool search is enabled for `gpt-6-astra`

**Fixed** — `gpt-6-astra` now negotiates the upstream's own tool-search capability instead of
falling back to the translated form. See [Effort & Context](/guides/effort-and-context/).

## 0.45.0 — 2026-09-14

### Admin JSON and mutation routes moved to `/admin/api/*`

**Changed · Breaking, effective 0.45.0** — Affects every scripted caller of the admin API. All 13
JSON and mutation routes gained one `/api` segment after `/admin`, and the old paths were removed
rather than aliased, so each caller must be updated. `/admin`, `/admin/login` and
`/admin/oidc/callback` are unchanged — the full before/after table is in
[Admin path migration](/reference/endpoints/#admin-path-migration).

### `GET /admin` returns the SPA shell instead of redirecting

**Changed · Breaking, effective 0.45.0** — Affects scripts that read the `303` to `/admin/login` as
"not signed in". An unauthenticated `GET /admin` now answers `200` with the embedded shell, which is
one static file identical for every visitor and carries no operator data; the redirect moved into the
bundle, which follows a `401` from `GET /admin/api/session`. Read that bootstrap endpoint to test for
a session. See [HTTP Endpoints](/reference/endpoints/#admin-spa-bundle---features-ui).

### Read keys can open a browser session

**Changed · Breaking, effective 0.45.0** — Affects deployments that relied on "read keys cannot open a
browser session" as a revocation property. `POST /admin/login` now answers `303` with a read-tier
session cookie for a `[server.admin] read_keys` credential where it previously answered `401`; every
mutation still answers `403`, so the cookie grants no permission the key lacked, but it carries its own
lifetime. Because browser sessions validate against the in-memory store alone, rotating a compromised
read key stops its header credential at the next reload while its cookie reads on until
`session_ttl_secs` (default 1h) elapses — restart rather than reload to drop it.

### Embedded admin SPA behind `--features ui`

**Added** — The admin dashboard is now a compiled SPA bundle embedded at build time and served from
`/admin`, replacing the server-rendered string literals. A binary built without `--features ui`
answers `404` on the shell routes, naming the feature. See
[Admin & Remote Provisioning](/guides/admin-remote-provisioning/).

### Graceful shutdown drain is bounded and configurable

**Added** — After the first SIGTERM/SIGINT, active HTTP/SSE/WebSocket work drains for
`shutdown_timeout_seconds` (default `30`, range `1`–`3600`) before the remainder is cancelled.
Changing it requires a restart. See [`[server]`](/reference/configuration/#server).

### `h2` updated for an empty-frame denial of service

**Security** — The `h2` dependency was updated to remediate a denial of service reachable by flooding
a connection with empty frames. Upgrade to 0.45.0 or later; no configuration change is needed.

### Optional function parameters survive translation

**Fixed** — OpenAI backends normalize function tools without an explicit `strict` toward strict mode,
where a closed parameter object makes every property behave as required and the model fills in values
the caller never set. shunt now pins `strict:false` on forwarded function tools, keeping optional
properties optional. See [Troubleshooting](/reference/troubleshooting/).

### Codex catalog failures use the negotiated error shape

**Fixed** — A model-discovery failure against the Codex catalog now returns the error shape the inbound
endpoint negotiated rather than always the Anthropic one, so OpenAI-protocol clients parse it through
their own error path. See [Model Discovery](/guides/model-discovery/).

### Admin login and provisioning fixes

**Fixed** — The login page CSP no longer sends `script-src` and `connect-src`; the SPA shell is served
on `/admin/` as well as `/admin`; concurrent completions for one pending login are serialized; a
refused start now says that it closed the authorization step; and the add-account form no longer
misreports a login.

## 0.44.0 — 2026-09-09

### Inbound Responses requests translate to Anthropic and Chat Completions upstreams

**Added** — A request arriving on the inbound Codex endpoint can now be routed to an Anthropic-kind or
Chat Completions upstream, with the translation handled in both directions. See
[Inbound Codex Endpoint](/guides/inbound-codex-endpoint/).

### In-stream `codex.rate_limits` is recorded on the WebSocket transport

**Fixed** — The WebSocket transport now records the in-stream `codex.rate_limits` event, so quota
windows stay current on reused connections instead of only on fresh HTTP turns. This feeds
[`GET /usage`](/reference/endpoints/).

### Tool-schema regex patterns the OpenAI validator rejects are dropped

**Fixed** — OpenAI backends compile every `pattern` in a tool schema with Python `re` and reject
JavaScript-only regexes (`\p{Cc}`, `(?<name>…)`, `\u{…}`). shunt now strips those patterns from
forwarded schemas; outside strict mode `pattern` is advisory, so only the hint is lost. See
[Troubleshooting](/reference/troubleshooting/).

## 0.43.0 — 2026-09-08

### `GET /usage` reports a per-provider breakdown

**Added** — Alongside the pool-wide aggregate, `GET /usage` now carries the same sanitized figures per
pooled provider under `providers`, so a client routing to one provider reads that provider's headroom
instead of the blended mean. Providers whose auth mode is not pooled are omitted. See
[HTTP Endpoints](/reference/endpoints/).

### `GET /usage` reports mean pool headroom, not the least-utilized account

**Changed** — Each window now reports `mean(1 - utilization)` across non-disabled accounts reporting
it — the fraction of the pool's combined capacity still unused — rather than the single least-utilized
account. A client reading the old field as one account's headroom will now see a pool-wide figure.

## 0.42.0 — 2026-09-07

### Model-routed third-party upstreams on the inbound Responses endpoint

**Added** — The inbound Responses endpoint now honors per-model routing to third-party upstreams, so a
Codex CLI client can reach a vendor other than OpenAI by model id. See
[Inbound Codex Endpoint](/guides/inbound-codex-endpoint/).

## 0.41.3 — 2026-09-07

### A Grok product without `usagePercent` no longer blanks the quota row

**Fixed** — A product reported with no `usagePercent` previously blanked the whole quota row instead of
being skipped. See [xAI / Grok](/guides/xai/).

## 0.41.2 — 2026-09-07

### The device page's SSO form can redirect to the identity provider

**Fixed** — The device page's CSP `form-action` refused the redirect to the configured identity
provider, so SSO could not complete from that page. See [Gateway Login](/guides/gateway-login/).

## 0.41.1 — 2026-09-07

### The device page's own form POST is accepted despite a null `Origin`

**Fixed** — `Referrer-Policy: no-referrer` makes a same-page form POST arrive with `Origin: null`,
which the CSRF guard refused. The guard now decides on `Sec-Fetch-Site: same-origin` first.

### An undelivered `agy` handoff no longer fails a completed turn

**Fixed** — A turn that had already completed is no longer failed by a handoff that could not be
delivered to the local `agy` subprocess. See [Antigravity](/providers/antigravity/).

## 0.41.0 — 2026-09-05

### Zhipu and MiniMax China presets

**Added** — `zhipu` and `minimax-cn` ship as built-in Anthropic-compatible presets, so the mainland
China endpoints need only a credential rather than a hand-written provider table. See
[Zhipu](/providers/zhipu/) and [MiniMax China](/providers/minimax-cn/).

### The Antigravity effort matrix is re-discovered for an unknown model

**Fixed** — A turn naming a model absent from the cached effort matrix now triggers re-discovery
instead of failing. See [Antigravity](/providers/antigravity/).

### Cursor composer fast mode and built-in tool calls

**Fixed** — Composer fast mode is sent as model metadata rather than encoded into the model id, and a
turn containing a built-in tool call is surfaced instead of dropped. See [Cursor](/providers/cursor/).

## 0.40.2 — 2026-09-05

### Antigravity rejects caller-supplied tools instead of ignoring them

**Changed** — The Antigravity upstream drives its own tool loop, so caller-supplied tools are now
refused with an error rather than silently dropped from the request. See
[Antigravity](/providers/antigravity/).

### Codex client identity bumped to 0.153.3 for `gpt-6-astra`

**Fixed** — The advertised Codex client identity was raised to 0.153.3, which the backend requires to
serve `gpt-6-astra`. See [ChatGPT / Codex](/guides/codex/).

### In-stream `rate_limit_exceeded` is classified as 429

**Fixed** — An in-stream `rate_limit_exceeded` event is now classified as a 429 and a misalignment
steer is forwarded to the client rather than discarded.

### Gemini tuple-style array schemas

**Fixed** — The `items` schema Gemini requires is now derived from a tuple-style array definition
instead of being sent in a form the backend rejects.

## 0.40.1 — 2026-09-03

### Antigravity resolves model ids from the live catalog

**Fixed** — Model ids are resolved against the account's live catalog, and a `base_url` pinned to the
production host is redirected, which together clear the spurious 429 "quota" refusals that made the
provider unusable. Antigravity's catalog is per-account and changes without notice, so ids are no
longer hardcoded. See [Antigravity](/providers/antigravity/).

## 0.40.0 — 2026-09-02

### Codex account quota from the `wham` usage endpoint

**Added** — shunt polls the `wham` usage endpoint for Codex account quota, so pool state is populated
without waiting for traffic to return `x-codex-*` headers. See
[Codex Multi-Account](/guides/codex-multi-account/).

### Antigravity reaches the daily backend with the agent envelope

**Fixed** — Requests now carry the full agent envelope and effort-suffixed model ids against the daily
backend, the combination the service actually accepts. See [Antigravity](/providers/antigravity/).

### Adjacent Gemini user turns are merged

**Fixed** — Adjacent user turns are merged so tool pairing survives a mid-conversation system message.

## 0.39.2 — 2026-09-01

### A never-selected account keeps its needs-relogin verdict

**Fixed** — An account no provider table has ever selected now reports `needs_relogin` alongside
`has_state: false`, because the admin refresh probe records its verdict by store name. See
[HTTP Endpoints](/reference/endpoints/).

## 0.39.1 — 2026-08-31

### Claude account status is reported by credential kind

**Fixed** — Account status is derived from the credential kind rather than raw expiry, so a dead
credential is reported as needing re-login instead of merely expired. See
[Anthropic Multi-Account](/guides/anthropic-multi-account/).

## 0.39.0 — 2026-08-29

### Stale near-quota accounts are re-probed opportunistically

**Added** — An account paused near its quota is re-probed when the opportunity arises, so it returns to
rotation as soon as its window resets rather than waiting out a fixed cooldown. See
[Anthropic Multi-Account](/guides/anthropic-multi-account/).

### Pool plans are keyed by account identity

**Fixed** — Plans are keyed by account identity and the backing file read is single-flighted, so
concurrent readers no longer attribute one account's plan to another.

### Deferred tools are stripped on non-Anthropic Messages models

**Fixed** — Deferred-tool blocks are removed before forwarding to a non-Anthropic Messages upstream,
which does not understand them. See [Model Aliases](/guides/model-aliases/).

### Reset-less quota marks have a bounded lifetime

**Fixed** — A quota mark that arrives without a reset timestamp now expires on its own instead of
pausing the account indefinitely.

## 0.38.0 — 2026-08-25

### `kind = "antigravity"` is the native HTTP upstream

**Changed · Breaking, effective 0.38.0** — Affects any config with an `antigravity` provider. The name
now means the native HTTP upstream; the local `agy` subprocess transport moved to
`kind = "antigravity_cli"` (built-in provider `antigravity-cli`). A config still carrying the old
meaning is refused by name rather than retargeted, and a routed `antigravity` provider with no
credential refuses to start. Rename the table to `antigravity_cli` to keep the subprocess transport,
or add a credential to adopt the HTTP one — see [Antigravity](/providers/antigravity/).

### `kind = "antigravity_cli"` is deprecated

**Deprecated** — The local `agy` subprocess transport is deprecated in favor of the native HTTP
upstream (`kind = "antigravity"`). It still works; migrate when convenient. See
[Antigravity](/providers/antigravity/).

### A shared credential slot holding a shunt credential is removed entirely

**Changed · Breaking, effective 0.38.0** — Affects only callers that send `authorization` or
`x-api-key` more than once in a single request. When shunt's own credential shares a slot with a
genuine upstream credential, the whole slot is now removed, so the upstream credential is dropped with
it. Send the upstream credential in a slot of its own. See
[Sharing a Gateway](/guides/shared-gateway/).

### Read/write admin keys, and the spend surface moved to `[server.spend]`

**Added** — `[server.admin]` gained `read_keys` and `write_keys`: a read key passes every admin GET and
is refused with `403` on every mutation. The spend-limit surface moved to its own
[`[server.spend]`](/reference/configuration/#serverspend-optional) table. See
[`[server.admin]`](/reference/configuration/#serveradmin-optional).

### `shunt gateway` login, token helper, and Claude Code launcher

**Added** — `shunt gateway login`, `shunt gateway token`, `shunt gateway claude` and
`shunt gateway logout` let a client authenticate against a shared gateway over a device flow and launch
Claude Code against it. See [Gateway Login](/guides/gateway-login/) and the [CLI](/reference/cli/).

### Kimi Code OAuth as a first-class subscription upstream

**Added** — A Kimi Code subscription can be used directly over OAuth, rather than only a Moonshot API
key. See [Kimi](/providers/kimi/).

### `[server.gateway.session]` JWT configuration

**Added** — Gateway session JWT parameters are configurable under
[`[server.gateway.session]`](/reference/configuration/#servergatewaysession-optional).

### `${VAR}` and `${file:}` references in config, with redacted secrets

**Added** — Config values resolve `${VAR}` environment and `${file:…}` path references, and secret
fields are redacted from debug output so a dumped config cannot leak credentials. See
[Configuration Reference](/reference/configuration/).

### Spend-limit admin API

**Added** — The first stage of the spend-limit admin API landed under
[`[server.spend]`](/reference/configuration/#serverspend-optional).

### Account plans exposed in pool state

**Added** — Pool account objects may carry an optional `plan` string, refined toward a more precise
value by a profile lookup when one is available. See [HTTP Endpoints](/reference/endpoints/).

### Codex client surface synced to `openai/codex` 0.148.0

**Changed** — The Codex client identity and request surface were synced to upstream 0.148.0. See
[ChatGPT / Codex](/guides/codex/).

### Gateway JWTs and client tokens never reach an upstream

**Security** — A gateway JWT is now stripped by shape rather than only when it authenticates, and is
never forwarded in either credential slot; a static `[server.auth]` token is stripped by value; and an
inbound `x-api-key` is stripped on the Codex passthrough. Every credential-slot forward site routes
through one shared strip, so the accept and strip rules cannot drift apart. See
[Sharing a Gateway](/guides/shared-gateway/).

### `shunt check` runs the routed-Antigravity credential guard

**Fixed** — `shunt check` now applies the same credential guard that startup does, so a routed
`antigravity` provider missing its credential is reported before the server is started. See the
[CLI](/reference/cli/).

## 0.37.0 — 2026-08-13

### Opt-in upstream Statuspage polling

**Added** — `[server.status]` polls configured provider Statuspage sources and exposes the most recent
indicator, description and incidents for observation. It is never consulted by routing or failover.
See [`[server.status]`](/reference/configuration/#serverstatus-optional).

### `grok-4.6` and a refreshed Grok model surface

**Added** — `grok-4.6` was added and the Grok model surface refreshed. See [xAI / Grok](/guides/xai/).

## 0.36.0 — 2026-08-11

### `agy` runs in agentic mode with streaming and sandboxing

**Added** — The Antigravity CLI transport runs in agentic mode with streamed output, sandboxing, and a
discovered effort matrix. See [Antigravity](/providers/antigravity/).

## 0.35.0 — 2026-08-10

### The inbound body cap defaults to 32 MiB

**Changed · Breaking, effective 0.35.0** — Affects anyone sending request bodies between 32 and 64 MiB,
typically large file or image requests. The cap now defaults to 32 MiB instead of the previous
hardcoded 64 MiB, and requests in that range return `413 request_too_large`. Raise
`[server.limits] max_request_bytes` to restore the old ceiling — see
[`[server.limits]`](/reference/configuration/#serverlimits).

### HTTP tuning configuration surface

**Added** — `[server.limits]`, `[server.timeouts]` and related tables expose body, header, URL and
timeout tuning that was previously hardcoded. See [Configuration Reference](/reference/configuration/).
