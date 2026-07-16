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

OAuth failures use the RFC 6749/RFC 8628 `{"error":"..."}` body. The existing
`/v1/messages`, `/v1/messages/count_tokens`, and `/v1/models` surfaces accept a
valid issued bearer token when gateway mode is enabled and keep their Anthropic
error envelope on authentication failure. If `[server.auth]` is also configured,
either its static client token or a valid gateway JWT grants access.

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
```

```bash
export SHUNT_GATEWAY_JWT_SECRET="$(openssl rand -base64 48)"
export SHUNT_GATEWAY_USERS='alice@example.com:<secret>,bob@example.com:<secret>'
```

Startup fails closed if `public_url` is not a bare HTTP(S) origin, the token TTL
is zero, the JWT secret is shorter than 32 bytes, or the users variable is empty
or malformed. Secret and user changes are re-resolved by config hot reload.
Whether the routes exist is fixed at boot, so adding or removing
`[server.gateway]` requires a restart.

## Pluggable approval

The HTTP endpoints depend on the `ApprovalProvider` trait rather than on the
static-user implementation directly. M-A ships `StaticUsers`, which resolves
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
in-memory stores. They survive a config hot reload but are not written to disk.
Restarting shunt therefore invalidates outstanding device codes and refresh
tokens, and every gateway user must sign in again. File persistence and
multi-instance coordination are follow-ups; M-A deliberately adds no database.

Use TLS for a non-loopback deployment. If a reverse proxy supplies
`X-Forwarded-For`, it must strip any client-supplied forwarding header before
adding its trusted value because the device verification limiter uses the first
forwarded address.

A gateway login session also has the reference gateway's reduced Claude Code
feature set: WebSearch is disabled, first-party-only beta headers and the
one-hour cache-TTL beta are omitted, and sign-in requires a browser. Personal
single-user installations that do not need managed identity should continue to
use `ANTHROPIC_BASE_URL` and, when needed, `[server.auth]`.

## Follow-ups

- **M-B:** authenticated `GET /managed/settings`, policy matching, ETag/304, and
  the managed telemetry environment push.
- **M-C:** authenticated inbound OTLP `POST /v1/{metrics,logs,traces}` sink and
  optional verbatim relay.
