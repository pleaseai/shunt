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

Three paths exist because the Codex CLI always appends `/responses` to whatever `base_url` it is
pointed at: a base ending in `/backend-api/codex` produces `/backend-api/codex/responses` (the
literal path the real ChatGPT backend uses), a base ending in `/v1` produces `/v1/responses`, and
a bare base produces `/responses`. Registering all three lets an operator use either CLI setup
style (§ "Codex CLI setup" below) without shunt needing to know which one a given client chose.

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
identity (`originator=codex_cli_rs`, `user-agent`/`version=codex_cli_rs/0.144.1`,
`OpenAI-Beta: responses=experimental`, and session/window headers derived from the session id); the
inbound passthrough does **not** — it forwards whatever `version`, `originator`, `user-agent`,
`OpenAI-Beta`, `session-id`, `thread-id`, `x-codex-window-id`, `x-codex-*`, `content-type`, and
`accept` the Codex CLI sent, so the client's **real** version drives the backend's
`minimal_client_version` model gating (see
[`codex-configuration.md` §5](codex-configuration.md#5-model-slugs)) exactly as if it were talking
to `chatgpt.com`. The only request headers shunt changes are:

- **Swapped in** per selected pool account: `Authorization: Bearer <account>` and
  `chatgpt-account-id` (replacing whatever the client sent).
- **Stripped**: the shunt client-token header (default `x-shunt-token`, so it never leaks upstream),
  the client's own `Authorization`/`chatgpt-account-id` (replaced above), `accept-encoding` (so the
  upstream body stays uncompressed for a clean byte relay), and framing/hop-by-hop headers the HTTP
  client recomputes (`host`, `content-length`, `connection`, …).

On the response side, only framing/hop-by-hop headers (`content-length`, `content-encoding`,
`transfer-encoding`, `connection`, …) are dropped so axum can frame the streamed body; every other
upstream header is relayed verbatim.

## Inbound authentication

Gated by `[server.auth]`, because the provider injects a server-side Codex bearer that must not be
handed to an unauthenticated caller. The client presents the shunt client token through the
configured header (default `x-shunt-token`), exactly as for `/v1/messages`. Without a configured
`[server.auth]`, the endpoint is open — acceptable for loopback or personal use, not for a shared
gateway.

Critically, the client's own `Authorization: Bearer` header (whatever the Codex CLI happens to
send) is **never used as the inbound credential and never forwarded upstream**, and the shunt
client-token header is **stripped** so it never leaks to the backend. The passthrough forwards the
Codex CLI's own request headers verbatim (see [Header passthrough](#header-passthrough) below) but
**swaps in only** the selected pool account's `Authorization` bearer + `chatgpt-account-id` — see
[`codex-configuration.md` §4.4](codex-configuration.md#4-authentication-codexauthjson). Nothing
about the client's own credential reaches the Codex backend.

## Account pool reuse (M10)

Session-sticky selection, reactive failover, cooldowns, and per-account refresh are all reused
unchanged from [M10](m10-codex-multi-account.md) — this endpoint adds no new pool logic, only a
new entry point into it.

- **Sticky key.** The Codex CLI's own `session-id` request header selects the sticky account (same
  hashing scheme as M10), falling back to `x-claude-code-session-id` for parity with the outbound
  path. Same session id → same account, for as long as that account stays healthy.
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
receiving an upstream response`. This is Anthropic-shaped — the one gateway-owned error on this
otherwise-passthrough path, since a Responses client has no better format to hand it than the same
shape `/v1/messages` uses for its own gateway-owned errors.

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

**1. Mirror the ChatGPT base URL** (keeps ChatGPT-subscription auth mode in the CLI):

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
