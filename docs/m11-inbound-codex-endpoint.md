# M11 — Inbound Codex endpoint (Codex CLI → shunt account pool)

M11 adds an opt-in **inbound** OpenAI Responses (Codex) endpoint so the OpenAI **Codex CLI**
itself can point its `base_url` at shunt and be load-balanced across a pool of ChatGPT/Codex OAuth
accounts. Every prior milestone routes traffic the other direction: [M1](m1-responses-translation.md)
translates *Claude Code's* Anthropic Messages requests into the Responses shape shunt sends
upstream, and [M10](m10-codex-multi-account.md) pools the accounts that outbound path uses. M11
is the reverse-facing counterpart — a Codex CLI client talks the Responses protocol directly to
shunt, and shunt relays it untranslated to the same M10 account-pool machinery.

## Contrast with `/v1/messages`

The existing `/v1/messages` path (Claude Code → shunt → Codex) and this endpoint (Codex CLI →
shunt → Codex) share an upstream but differ in kind:

| | `/v1/messages` (outbound, existing) | inbound Codex endpoint (this milestone) |
| :-- | :-- | :-- |
| Inbound client | Claude Code (Anthropic Messages) | OpenAI Codex CLI (OpenAI Responses) |
| Inbound → upstream body | **Translated**: `translate_request` builds a Responses body from the Anthropic Messages request | **Raw passthrough**: the inbound Responses body is forwarded upstream byte-for-byte, no translation |
| Upstream → outbound response | **Re-shaped**: `AnthropicSseMachine` turns Responses SSE into Anthropic SSE (or a single Anthropic JSON body) | **Raw passthrough**: the upstream response (SSE or JSON) is relayed verbatim, preserving status and content-type |
| On pool exhaustion | Re-shapes the last upstream response into an Anthropic-style error envelope (`build_upstream_error`) | Relays the last upstream response verbatim — **not** re-shaped (see below) |
| Model selects provider? | Yes, via `[[routes]]` / `[[route_prefixes]]` | No — every request goes to the one configured provider; `model` forwards verbatim as a label only |

Everything else — the M10 account pool, session-sticky selection, cooldowns, and refresh — is
shared unchanged between the two paths.

## Configuration

A new opt-in `[server.codex_endpoint]` table, mirroring the [M9](m9-admin-surface.md)
`[server.admin]` opt-in pattern:

```toml
[server.codex_endpoint]
provider = "codex"   # default; the target chatgpt_oauth provider
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `provider` | `"codex"` | Which `chatgpt_oauth` provider's account pool serves inbound Responses requests. |

**Absent ⇒ none of the routes are registered** — the default HTTP surface is unchanged. Present ⇒
config validation requires the named provider to exist and use `auth = "chatgpt_oauth"`; otherwise
shunt fails to start with a `ConfigError` naming the problem (unknown provider, or wrong auth
mode). This is the same bearer-leak discipline M8/M10 apply elsewhere: only a `chatgpt_oauth`
provider has the Codex OAuth injection this endpoint depends on.

## Routes

When opted in, shunt registers three routes, all mapping to one passthrough handler:

| Method | Path |
| :-- | :-- |
| `POST` | `/backend-api/codex/responses` |
| `POST` | `/responses` |
| `POST` | `/v1/responses` |

Three Responses paths exist because the Codex CLI always appends `/responses` to whatever `base_url` it is
pointed at: a base ending in `/backend-api/codex` produces `/backend-api/codex/responses` (the
literal path the real ChatGPT backend uses), a base ending in `/v1` produces `/v1/responses`, and
a bare base produces `/responses`. Registering all three lets an operator use either CLI setup
style (§ "Codex CLI setup" below) without shunt needing to know which one a given client chose.

### Client analytics sink

The same opt-in also registers `POST /backend-api/codex/analytics-events/events` and
`POST /codex/analytics-events/events`, covering the default ChatGPT-style and root-style
`chatgpt_base_url` forms. The Codex CLI posts JSON batches shaped as
`{"events":[{"event_type":"...","event_params":{...}}]}`. shunt authenticates these routes the
same way as the Responses routes, accepts the body, and always returns `200` with `{}` after the
auth check. It never forwards the batch: selecting one pooled account would misattribute the
client's product telemetry to that account.

The body and event properties are discarded without logging or export. Only a per-event-name
`shunt.codex_client_events` counter is emitted to the already opt-in Sentry/OpenTelemetry metric
sinks. Event names are limited to 64 bytes and to lowercase ASCII letters, digits, `.`, `_`, and
`-`; invalid names become `other`, while malformed or unrecognized batches are counted once as
`unparsed`. Oversized or unreadable bodies also succeed and are counted as `unparsed`. With no
metric sink configured, these routes are pure discard sinks.

## Fixed provider routing

Unlike `/v1/messages`, this endpoint does not route by model. Every inbound request goes to the
**one** provider named in `[server.codex_endpoint]`. The inbound body's `model` field is forwarded
upstream verbatim — it is read only for metrics/logging labels, never used to pick a provider — so
a request naming a model the account pool's ChatGPT subscription isn't entitled to fails exactly
the way it would talking to the real ChatGPT backend directly (see
[`codex-configuration.md` §5](codex-configuration.md#5-model-slugs)).

## Raw passthrough

The inbound Responses body is forwarded upstream **byte-for-byte** — no `translate_request`, no
model/effort resolution, no field rewriting of any kind. The upstream response is relayed back
**verbatim**: the status code and (almost) every upstream response header are preserved unchanged,
so an SSE reply stays `text/event-stream`, a non-streaming reply stays a single `application/json`
body, and headers like `retry-after` and `x-codex-turn-state` reach the CLI untouched. There is no
`AnthropicSseMachine`, no keepalive-ping injection, and no error re-shaping on a normal request —
the Codex CLI speaks the same wire protocol to shunt that it would speak directly to
`chatgpt.com`.

### Header passthrough

Because the inbound client **is** a real Codex CLI (unlike the `/v1/messages` path, where shunt
*impersonates* one), the passthrough forwards the client's **own request headers verbatim** rather
than synthesizing them. shunt's translating path builds a fresh request with a hardcoded Codex
identity (`originator=codex_cli_rs`, `user-agent=codex_cli_rs/0.144.4`, `version=0.144.4`,
`OpenAI-Beta: responses=experimental`, and session/window headers derived from the session id); the
inbound passthrough does **not** — it forwards whatever `version`, `originator`, `user-agent`,
`OpenAI-Beta`, `session-id`, `thread-id`, `x-codex-window-id`, `x-codex-*`, `content-type`, and
`accept` the Codex CLI sent, so the client's **real** version drives the backend's
`minimal_client_version` model gating (see
[`codex-configuration.md` §5](codex-configuration.md#5-model-slugs)) exactly as if it were talking
to `chatgpt.com`. The only request headers shunt changes are:

- **Swapped in** per selected pool account: `Authorization: Bearer <account>` and
  `chatgpt-account-id` (replacing whatever the client sent).
- **Stripped**: the shunt client-token header (the default `x-shunt-token` is stripped unconditionally
  — even on an ungated endpoint, or one using a custom auth header — so it never leaks upstream),
  the client's own `Authorization`/`chatgpt-account-id` (replaced above), `accept-encoding` (so the
  upstream body stays uncompressed for a clean byte relay), and framing/hop-by-hop headers the HTTP
  client recomputes (`host`, `content-length`, `connection`, …).

On the response side, framing/hop-by-hop headers (`content-length`, `content-encoding`,
`transfer-encoding`, `connection`, …) are dropped so axum can frame the streamed body, and
`set-cookie`/`set-cookie2` are dropped so an upstream/edge session cookie (e.g. Cloudflare
`__cf_bm`/`cf_clearance`) bound to shunt's server-side egress is never leaked to the untrusted
inbound client (CWE-200/CWE-201); every other upstream header is relayed verbatim.

## Inbound authentication

Gated by `[server.auth]`, because the provider injects a server-side Codex bearer that must not be
handed to an unauthenticated caller. The client presents the shunt client token **either** through
the configured header (default `x-shunt-token`) **or** as an OpenAI-style `Authorization: Bearer
<token>` — the `OPENAI_API_KEY` / `env_key` idiom the Codex CLI (and LiteLLM/llmgateway proxy setups)
use, so no custom header is required. Both are checked by `InboundAuth::authenticate_bearer` (the
`/v1/messages` path uses the header-only `authenticate`; only this endpoint additionally accepts the
Bearer form, since its client is a Codex CLI). Without a configured `[server.auth]`, the endpoint is
open — acceptable for loopback or personal use, not for a shared gateway.

Critically, the client's `Authorization: Bearer` header is **never forwarded upstream**, whatever it
carries: if it holds the shunt client token it authenticates the request (via `authenticate_bearer`
above) and is then stripped; if it holds anything else (e.g. the Codex CLI's own ChatGPT credential)
it fails the inbound check and is likewise stripped. The shunt client-token header is **stripped**
too, so neither the shunt token nor the client's own credential ever leaks to the backend. The passthrough forwards the
Codex CLI's own request headers verbatim (see [Header passthrough](#header-passthrough) below) but
**swaps in only** the selected pool account's `Authorization` bearer + `chatgpt-account-id` — see
[`codex-configuration.md` §4.4](codex-configuration.md#4-authentication-codexauthjson). Nothing
about the client's own credential reaches the Codex backend.

## Account pool reuse (M10)

Session-sticky quota-aware selection (issue #195), reactive failover, cooldowns, storm control, per-account refresh, and identity coalescing are all reused unchanged from [M10](m10-codex-multi-account.md) — this endpoint adds no new pool logic, only a new entry point into it. Account resolution goes through the shared `resolve_pool_accounts` path: configured entries are used directly, while an empty list uses the store directory-mtime cache and fills each scanned account's stable identity from its Codex `account_id`. The existing empty-store single-account fallback remains unchanged.

- **Sticky key.** The Codex CLI's own `session-id` request header selects the sticky account (same
  hashing scheme as M10), falling back to `x-claude-code-session-id` for parity with the outbound
  path. Same session id → same account, for as long as that account stays healthy. When
  `[server.auth]` is configured, the sticky key is namespaced with the authenticated client name
  (`{client}:{session-id}`, mirroring the outbound path) so that on a multi-tenant gateway one
  client cannot pin another client's session onto a chosen account by replaying its `session-id`.
- **Failover** follows M10's rules exactly: every `429` rotates (cooldown = `retry-after` clamped
  1–3600s, default 60s); a `5xx` or transport failure cools the account for 30s and rotates; a
  `401` on a refreshable (store/`credentials`) account triggers a force-refresh and one retry (5
  minutes + rotate if still failing); a `401` on a `token_env` account (not refreshable) cools for
  5 minutes and rotates; an unresolvable credential cools for 5 minutes and rotates.
- **Success.** A successful pooled response clears that account's cooldown and carries an
  `x-shunt-account: <name>` response header, same as the outbound path — use a neutral account
  label on a shared gateway (see M10's header note).

## Exhaustion behavior (differs from `/v1/messages`)

When every account in the pool has been tried and **at least one** upstream response was
received, shunt relays the **last** upstream response **verbatim** — status and body unchanged.
This is the opposite of the `/v1/messages` Codex path, which re-shapes the last response into an
Anthropic-style error envelope (`build_upstream_error`); a passthrough client expects the raw
Responses-shaped body it would have gotten from `chatgpt.com` directly, error or not.

If every account fails **before** any upstream response is received at all (for example, every
account's credentials are unresolvable), there is no real upstream body to relay, so shunt returns
a gateway-owned `502 bad gateway` with the fixed message `all Codex OAuth accounts failed before
receiving an upstream response`.

Every **gateway-owned** error on this endpoint — this 502, the inbound-auth `401`, an oversized or
unreadable request body, a Codex endpoint disabled by hot reload, or an account-store scan failure — is returned
in the **OpenAI Responses error shape** (`{"error":{"message":..,"type":..,"code":null}}`),
preserving its status code, so a Codex CLI (or any OpenAI Responses client) parses it through its
own error path rather than the Anthropic `{"type":"error",...}` envelope shunt uses elsewhere. This
is the one deliberate exception to the byte-for-byte passthrough: relayed **upstream** errors
(429/4xx/5xx from the backend) still pass through verbatim and unchanged. The re-shaping happens
once, at the endpoint boundary (`codex_endpoint::post` → `error::into_openai_error_shape`), so the
Anthropic Messages path keeps its own error shape untouched (issue #127).

## Single-account fallback

If the configured provider has no `[[accounts]]` and the account store is also empty, shunt falls
back to the single default `~/.codex/auth.json` / `$CODEX_AUTH_FILE` credential — no pool, no
failover, no `x-shunt-account` header — mirroring M10's existing single-account behavior on the
outbound path. A user with one Codex login therefore works out of the box the moment
`[server.codex_endpoint]` is set, with no account configuration at all.

## Transport: HTTP/SSE only

Even if the configured provider sets `websocket = true`, this endpoint always uses the HTTP path.
The experimental [Codex WebSocket v2 transport](codex-websocket-v2-protocol.md) is out of scope for
M11 and is tracked as a follow-up (see below).

## Reload behavior

Like `[server.admin]`, the route set is decided **once at boot** from the initial config — a
config reload cannot add or drop these routes. A reload *can* change which provider
`[server.codex_endpoint].provider` names; toggling the table on/off at runtime instead logs a
warning that a restart is required to register or drop the routes.

## Codex CLI setup

Point the Codex CLI at shunt with one of two `~/.codex/config.toml` shapes:

**1. Mirror the ChatGPT base URL** (keeps ChatGPT-subscription auth mode in the CLI — the CLI keeps
sending its own ChatGPT credential, so this shape still needs the CLI's local `~/.codex/auth.json`
login and only works against an **ungated** endpoint; with `[server.auth]` set, that ChatGPT bearer
is not the configured shunt token and is rejected, so use shape 2 there):

```toml
chatgpt_base_url = "http://<shunt-host>:3001/backend-api/codex"
```

The CLI appends `/responses`, landing on shunt's `/backend-api/codex/responses` route.

**2. A custom model provider** (selected via the CLI's `model_provider` setting):

```toml
[model_providers.shunt]
base_url = "http://<shunt-host>:3001/v1"
wire_api = "responses"
```

The CLI appends `/responses`, landing on `/v1/responses`.

When `[server.auth]` is configured on shunt, add the shunt client token as a header the CLI sends
— e.g. on the custom `model_providers.shunt` table:

```toml
[model_providers.shunt]
base_url = "http://<shunt-host>:3001/v1"
wire_api = "responses"
http_headers = { "x-shunt-token" = "<token>" }
```

A loopback `base_url` may stay plain `http://` (shunt allows loopback over plaintext); use
`https://` for anything remote. The Codex CLI's own ChatGPT login is irrelevant once pointed at
shunt this way — shunt supplies the account from its own pool, not the CLI's local
`~/.codex/auth.json`. Provision pool accounts the same way as the outbound path:
`shunt login codex --name <name>` (see
[`codex-configuration.md` §12](codex-configuration.md#12-multi-account-pooling)).

## Out of scope / follow-up

- **WebSocket transport.** This endpoint is HTTP/SSE-only even when the target provider has
  `websocket = true`; wiring the inbound path onto the
  [Codex WebSocket v2 transport](codex-websocket-v2-protocol.md) is a separate follow-up.
- **Model-based provider selection.** The endpoint is pinned to one provider by config; routing
  inbound Responses requests to different providers by `model` (mirroring `[[routes]]`) is not
  implemented and would need its own design (this endpoint has no Anthropic-shaped request to key
  routing decisions off of the way `/v1/messages` does).
- **Admin surface integration.** [M9's](m9-admin-surface.md) dashboard reports `claude_oauth` pool
  health only; extending it to show inbound-Codex-endpoint traffic is a separate follow-up.
