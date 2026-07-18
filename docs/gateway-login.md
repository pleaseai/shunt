# M-A — Claude apps gateway login

## Scope

M-A adds the OAuth 2.0 device-flow surface that lets Claude Code sign in to
shunt with managed `forceLoginMethod: "gateway"` settings. It is opt-in through
`[server.gateway]`; when that table is absent, shunt registers none of the new
login routes and its existing authentication behavior is unchanged.

Implemented endpoints:

| Endpoint | Contract |
| :-- | :-- |
| `GET /.well-known/oauth-authorization-server` | RFC 8414 metadata plus `gateway_protocol_version: 1` |
| `POST /oauth/device_authorization` | RFC 8628 device authorization; 256-bit opaque device code, base-20 `XXXX-XXXX` user code, 600-second lifetime, 5-second polling interval |
| `GET /device` | Browser approval form; a `user_code` query parameter only pre-fills the form and never auto-approves |
| `POST /device` | Same-origin CSRF guard, per-IP attempt limit, static-user authentication, and grant approval |
| `POST /oauth/token` | Device grant polling and rotating refresh grant |

OAuth failures use the RFC 6749/RFC 8628 `{"error":"..."}` body. For
routes whose selected provider injects a server-side credential, the existing
`/v1/messages` and `/v1/messages/count_tokens` surfaces accept a valid issued
bearer token when gateway mode is enabled; `/v1/models` does as well. Passthrough
providers remain open. Authentication failures keep the Anthropic error envelope.
If `[server.auth]` is also configured, either its static client token or a valid
gateway JWT grants access on those gated routes.

Successful device and refresh grants return the same shape:

```json
{
  "access_token": "<HS256 JWT>",
  "refresh_token": "<opaque rotating token>",
  "token_type": "Bearer",
  "expires_in": 3600
}
```

The JWT contains `sub`, `email`, `name`, `aud: "shunt"`, `iss`, `iat`, and
`exp`. It is signed with HS256 using the environment-backed secret configured by
`jwt_secret_env`. Refresh tokens are 256-bit opaque identifiers. Every successful
refresh rotates the token; replaying a used token revokes the active token in
that rotation family and returns `401 {"error":"invalid_grant"}`.

## Configuration

```toml
[server.gateway]
public_url = "https://gateway.example.com"
jwt_secret_env = "SHUNT_GATEWAY_JWT_SECRET" # default
users_env = "SHUNT_GATEWAY_USERS"           # default
token_ttl_seconds = 3600                     # default
trust_forwarded_for = false                  # default
# state_path = "~/.shunt/gateway-sessions.json"  # default; "" = memory-only sessions
```

```bash
export SHUNT_GATEWAY_JWT_SECRET="$(openssl rand -base64 48)"
export SHUNT_GATEWAY_USERS='alice@example.com:<secret>,bob@example.com:<secret>'
```

Startup fails closed if `public_url` is not a bare HTTPS origin (`http` is
accepted only on loopback), the token TTL is zero, the JWT secret is shorter than
32 bytes, or the users variable is empty or malformed. Secret and user changes are re-resolved by config hot reload.
Whether the routes exist is fixed at boot, so adding or removing
`[server.gateway]` requires a restart.

## Pluggable approval

The HTTP endpoints depend on the `ApprovalProvider` trait rather than on the
static-user implementation directly. `GatewayAuth::with_approval_provider`
accepts an `Arc<dyn ApprovalProvider>` for integrations, while M-A ships
`StaticUsers`, which resolves
comma-separated `email:secret` entries from `users_env`, compares secrets in
constant time, and emits an identity with `sub = email`, `email = email`, and
`name` set to the local part before `@`. A future OIDC provider can implement the
same trait without changing the device or token endpoints.

The browser form is server-rendered and uses no client-side script. Its mutation
is accepted only with a same-origin `Origin` or `Referer`, a same-origin/same-site
Fetch Metadata signal, or a browser-navigation `Sec-Fetch-Site: none` request
without contradictory cross-site hints. A rejected request returns HTTP 200 with
a human-readable blocked notice, matching the reference gateway behavior.

## State and operational boundary

Device grants, refresh tokens, and rate-limit counters are process-lifetime,
in-memory stores that survive a config hot reload. Mutating operations remove
expired device grants and idle rate-limit entries; both stores reject new
admission at 4,096 live entries. Used refresh-token tombstones are retained
for 30 days and capped at 64 per family, preserving bounded replay detection
without process-lifetime growth; active refresh tokens that go 30 days without
rotating expire the same way.

The refresh-token store additionally persists to `state_path` by default
(`~/.shunt/gateway-sessions.json` — the directory shunt's account stores
already use; issue #194), mirroring the pool quota cache
(`src/state_persist.rs`): the token endpoint writes the store — atomically,
owner-only permissions (0600 on Unix) — after every grant, rotation, or replay
revocation, before the response is sent, and boot restores it before serving,
so a restart keeps managed logins alive. Tokens are keyed by SHA-256 both in
memory and on disk (they are 256-bit random, so an unsalted hash suffices), so
the file never holds a usable credential — only token hashes, rotation-family
ids, timestamps, and the signed-in identities. Reading is best-effort: a
missing, corrupt, or version-mismatched file falls back to memory-only
behavior, never a boot failure. Setting `state_path = ""` keeps sessions
memory-only — then restarting shunt invalidates outstanding refresh tokens,
and existing access JWTs remain valid until expiry, after which users must
sign in again; an environment with no resolvable home directory behaves the
same. Device grants and rate-limit counters stay memory-only by design (a
restart mid-login only costs that attempt). The state file is single-process;
sharing grants and replay detection between concurrent gateway instances
remains a follow-up, and this change deliberately adds no database.

Refresh grants mint tokens from the identity stored with the session and do
not re-check the `users_env` approval list, so removing a user from
`users_env` does not end an existing session — with persistence default-on,
the session survives up to the 30-day idle horizon. To deprovision a user
immediately, also delete the state file (or set `state_path = ""`) and
restart.

Use TLS for a non-loopback deployment. By default `/device` rate limiting uses
the socket peer and ignores `X-Forwarded-For` and `X-Real-IP`. Set
`trust_forwarded_for = true` only when every request reaches shunt through a
trusted reverse proxy that strips client-supplied forwarding headers and adds
its own trusted client address. Enabling it on a directly exposed gateway lets
clients choose their rate-limit identity.

A gateway login session also has the reference gateway's reduced Claude Code
feature set: WebSearch is disabled, first-party-only beta headers and the
one-hour cache-TTL beta are omitted, and sign-in requires a browser. Personal
single-user installations that do not need managed identity should continue to
use `ANTHROPIC_BASE_URL` and, when needed, `[server.auth]`.

For per-user policy after sign-in, shunt now serves authenticated
`GET /managed/settings` with ordered email matching, `ETag`/`304`, telemetry
environment push, and `availableModels` enforcement. See the
[M-B managed-settings note](gateway-managed-settings.md).

## Follow-ups

- **M-C:** authenticated inbound OTLP `POST /v1/{metrics,logs,traces}` sink and
  optional verbatim relay.
- **Multi-instance session sharing:** move refresh sessions (and device grants)
  behind a shared backend (e.g. PostgreSQL) that owns the state, with atomic
  token rotation, so concurrent gateway instances agree on grants and replay
  detection. The store's narrow `issue`/`rotate`/`export`/`import` surface and
  epoch-based, hash-keyed records are designed to make that a contained
  swap; the `state_path` file stays the single-process default.
