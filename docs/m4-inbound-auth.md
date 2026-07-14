# M4 — Inbound client authentication (shared-gateway tokens)

## 0. Problem

shunt has no inbound authentication. For **passthrough** providers that is correct — the
caller's own Anthropic credential is forwarded, so every caller pays for themselves. But for
providers where shunt **injects a server-side credential** (`auth = "api_key"` or
`auth = "chatgpt_oauth"`), any client that can reach the listener spends the operator's
account. That is fine for a loopback-only personal gateway; it is not fine once the gateway
is shared with other people over a VPN / tunnel.

M4 adds an **optional, per-client token check** on exactly those injected-credential routes.
Transport security stays out of scope: shunt still serves plain HTTP and relies on the
deployment (WireGuard, Tailscale, Cloudflare Tunnel TLS, loopback) for encryption.

## 1. Configuration

New optional `[server.auth]` table. Absent ⇒ behavior unchanged (no inbound auth).

```toml
[server]
bind = "0.0.0.0:3001"
default_provider = "anthropic"

[server.auth]
# Header carrying the client token. Optional; default "x-shunt-token".
header = "x-shunt-token"
# Env var holding the client tokens. Optional; default "SHUNT_CLIENT_TOKENS".
# Value format: comma-separated `name:token` pairs, e.g.
#   SHUNT_CLIENT_TOKENS="minsu:3f9c…,alice:a41b…"
# Names are labels for logging only; tokens are the secrets.
tokens_env = "SHUNT_CLIENT_TOKENS"
```

Rules:

- Tokens live in the **environment**, never in the TOML (matches `api_key_env`).
- `[server.auth]` present but env var unset/empty ⇒ **startup error** (fail closed at boot,
  like the existing config validation — never silently run open when auth was requested).
- Parse errors (entry without `:`, empty name or token, duplicate name) ⇒ startup error.
- Token value = everything after the **first** `:` (tokens may contain `:`). Surrounding
  whitespace around entries is trimmed; whitespace inside a token is preserved.

## 2. Enforcement

Checked in the `/v1/messages` and `/v1/messages/count_tokens` handlers **after routing
resolves the provider**, and only when that provider's `auth` mode injects a server-side
credential (`ApiKey`, `ChatgptOauth`, `ClaudeOauth`, …). `GET /v1/models` is checked
whenever `[server.auth]` is configured, because it exposes the configured model list:

- `Passthrough` provider ⇒ no check (caller uses their own credential), regardless of config.
- Both the injected-credential inference gate and `GET /v1/models` accept the client token
  in any of the standard Anthropic credential slots, with an explicit priority when several
  carry valid tokens: the configured header (default `x-shunt-token`), then
  `Authorization: Bearer <token>`, then `x-api-key`. A Claude Code client on a pool-only
  gateway therefore authenticates with the `ANTHROPIC_AUTH_TOKEN` it already sends — no
  `ANTHROPIC_CUSTOM_HEADERS` line needed. (Before #130 the inference gate accepted only the
  configured header; discovery gained the wider set in #90.)
- `HEAD /` and `GET /routes` ⇒ never checked (`/` is liveness; `/routes` remains shunt-native metadata).
- Injected-credential route with valid token ⇒ proceed; log the client **name** (never the
  token) as a tracing field on the request span / relevant log lines.
- Missing or unknown token ⇒ `401` with an Anthropic-shaped error body:

```json
{"type":"error","error":{"type":"authentication_error","message":"missing or invalid credential: this gateway requires a client token (via x-shunt-token, Authorization: Bearer, or x-api-key) for mapped models; ask the operator for one"}}
```

  (message uses the configured header name; a `warn` log records the failure and the
  provider, never the presented token value.)

Safety boundary: a credential accepted as a gate token must never be forwarded upstream.
On a gated (injected-credential) route the gateway strips `authorization` and `x-api-key`
from the forwarded headers after a successful check — the injected-credential adapters
replace those headers with the provider credential anyway, but the boundary must not
depend on adapter behavior. Passthrough routes are never gated, so the client's `Bearer` /
`x-api-key` remain the real upstream Anthropic credential and are forwarded unchanged.
Operators mixing passthrough and mapped models on one shared gateway should keep handing
out dedicated `x-shunt-token` values: the `Bearer` slot then stays free to carry the real
Anthropic credential for passthrough models.

## 3. Comparison & hygiene

- **Constant-time comparison**, no new dependency: compare presented token against every
  configured token with a length check folded into a byte-XOR accumulator (compare against
  all entries even after a match to keep timing independent of position).
- The auth header is **always stripped** before forwarding upstream (add it to the strip
  logic beside `HOP_BY_HOP_HEADERS` in `src/headers.rs` — dynamic name, so a function that
  takes the configured header name rather than a const entry).
- Never log token values at any level, including debug.

## 4. Client setup (docs)

Document in `docs/running.md` (new §5 subsection) and `shunt.toml.example`:

```bash
# Claude Code side — ANTHROPIC_CUSTOM_HEADERS supports one "Name: Value" per line
export ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <your token>"
```

Note the composition guidance: transport encryption comes from the tunnel (WireGuard /
Tailscale / Cloudflare Tunnel); the token distinguishes and revokes **users** on top.

## 5. Tests

Pure unit tests (no network, no loopback bind — Codex-sandbox safe):

- token env parsing: happy path, token containing `:`, whitespace trimming, and the
  startup-error cases (missing env, empty, duplicate name, malformed entry).
- constant-time equality helper: equal / unequal / different-length.

Integration tests (wiremock, alongside the existing suites):

- mapped (injected-credential) route, auth configured, no token ⇒ 401, upstream never called.
- mapped route, wrong token ⇒ 401.
- mapped route, valid token ⇒ 200, upstream called, **auth header absent** from the
  forwarded request.
- passthrough route, auth configured, no token ⇒ still forwarded (unchanged behavior).
- auth not configured ⇒ mapped route works without a token (backward compat).

## 6. Out of scope

- TLS termination, OIDC/SSO (deployment-layer concerns; see running.md guidance).
- Per-client rate limits or spend accounting (a possible M5; the `name` label in logs is the
  hook for it).
- Interactive token minting — operators generate tokens themselves (e.g. `openssl rand -hex 32`).
