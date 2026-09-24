---
title: Configuration Reference
description: Every shunt.toml key — server, providers, routes, models.
---

The keys below are shown in TOML, but a config file may also be written in YAML (`shunt.yaml`/`shunt.yml`) — the schema is identical, only the syntax differs. See [Configuration](/guides/configuration/) for file locations, precedence, and an annotated example. Full template: [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).

## Secret references

Any string value in the config file may be written as `${VAR}` or `${file:/abs/path}` instead of a literal, so a secret doesn't have to sit in the file itself. Full details, including redaction and hot-reload interaction, are in [`docs/config-secrets.md`](https://github.com/pleaseai/shunt/blob/main/docs/config-secrets.md).

| Form | Resolves to | Constraints |
| :-- | :-- | :-- |
| `${VAR}` | Environment variable `VAR` | May be embedded in a longer string, e.g. `"Bearer ${TOKEN}"`. Config load fails if `VAR` is undefined |
| `${file:/abs/path}` | The file's contents, trimmed | Must be the field's entire value, and the path must be absolute. Config load fails if the file is unreadable, the path is relative, or the reference is embedded in a longer string |

`$${` escapes to a literal `${`. Resolution is not recursive — a resolved value is not re-scanned for further references. This applies only to the config file; `SHUNT_*` environment overrides are used as-is. It reruns on every config load, including [hot reload](https://github.com/pleaseai/shunt/blob/main/docs/config-reload.md), so a `${file:}`-backed secret can be rotated by rewriting the referenced file and triggering a reload, without restarting shunt. Whether the new value then takes effect follows the field's own reload behavior: `[sentry]` and `[otel]` are initialized once at startup, so rotating a secret in those two sections updates the config but needs a restart to apply.

Six field paths are additionally typed as a redacting secret and render as `[redacted]` in diagnostic output: [`[sentry] dsn`](#sentry-optional), [`[otel.headers]`](#otelheaders-optional) values, [`[server.gateway.telemetry] forward_to[].headers`](#servergatewaytelemetry-optional) values, [`[server.gateway.session] jwt_secret`](#servergatewaysession-optional), and the `key` of each [`[[server.admin.write_keys]]` and `[[server.admin.read_keys]]`](#serveradmin-optional) entry. For the first four, a literal value still works exactly as before; shunt additionally logs one advisory boot warning naming the affected field paths — never the values — suggesting `${VAR}` / `${file:...}`. The two admin key arrays are the exception: a literal there **fails config load** rather than warning, so an admin key never lives in the config file.

The existing `tokens_env`, `jwt_secret_env`, `client_secret_env`, `api_key_env`, `users_env`, `token_env`, and `tokens_file` fields are unaffected by this and keep naming an environment variable or file path, as before (`jwt_secret_env` is separately deprecated in favor of [`session.jwt_secret`](#servergatewaysession-optional)) — see each field's own entry below.

## `[server]`

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `bind` | `127.0.0.1:3001` | Address shunt listens on |
| `default_provider` | `anthropic` | Provider for any model with no matching route |
| `shutdown_timeout_seconds` | `30` | Seconds after the first SIGTERM/SIGINT for active HTTP/SSE/WebSocket work to drain before the remainder is cancelled. Must be `1`–`3600`; changing it requires a restart |
| `max_concurrent_requests` | `1024` | Maximum inbound requests in flight through response-body completion. Excess requests are shed immediately with `503` and `Retry-After: 1`; `0` disables the limit. `/` and `/health` are exempt. A restart is required after changing this key |
| `sse_keepalive_seconds` | `30` | Idle seconds before an SSE `ping` is injected; `0` disables ([details](/guides/shared-gateway/#sse-keepalive-pings)) |

## `[server.access_control]`

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `allow_cidrs` | `[]` | Allowed client CIDRs. A non-empty list makes other addresses default-deny. `/` and `/health` bypass this allow check only |
| `deny_cidrs` | `[]` | Denied client CIDRs. Deny is evaluated first and also applies to `/` and `/health` |
| `trust_forwarded_for` | `false` | Use the first `X-Forwarded-For` value, then `X-Real-IP`, instead of the connection peer. Enable only behind a trusted proxy that overwrites client-supplied forwarding headers |

CIDRs are validated by `shunt check`. A missing peer address is rejected when `allow_cidrs` is non-empty. Access-control changes require a restart because the router layer is installed at boot.

This `trust_forwarded_for` switch is independent of `[server.gateway] trust_forwarded_for`. The access-control switch affects only CIDR allow/deny rules; the gateway switch affects only the device-flow rate limiters. If both surfaces run behind a trusted reverse proxy, set both switches. Setting only one leaves the other surface using the socket peer address.

## `[server.limits]`

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `max_request_bytes` | `33554432` (32 MiB) | Maximum body bytes for Anthropic Messages and inbound Codex Responses requests. An oversized declared `Content-Length` is rejected before body buffering; chunked bodies use the same cap while being read. Returns `413 request_too_large`. Other gateway, admin, telemetry, and analytics routes retain their endpoint-specific limits. Hot-reloads |
| `max_request_header_bytes` | _(unset)_ | Maximum sum of parsed header-name and header-value lengths across all headers. This is not raw HTTP wire size. Returns `431`; changing it requires a restart |
| `max_url_length` | _(unset)_ | Maximum request URI string length, including the query. Returns `414`; changing it requires a restart |

`max_request_bytes` must be greater than zero. The optional limits must be greater than zero when set. The 32 MiB body default replaces the previous hardcoded 64 MiB limit; raise it for larger file or image requests.

## `[server.timeouts]`

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `upstream_ttfb_ms` | `120000` | Maximum wait for inference-upstream HTTP response headers; `0` disables. The response body then has no wall-clock cap, preserving long SSE streams. Turns that have not committed an SSE response return `504 timeout_error`; a committed streaming turn surfaces the timeout as one terminal SSE `error` event carrying the same envelope — the timeout never advances a chain. Hot-reloads |

This timeout covers the Anthropic Messages transports, OpenAI Responses HTTP transport (including WebSocket fallback), Gemini HTTP transport, and inbound Codex Responses passthrough. It does not cover Codex WebSocket turns, Cursor transports, Antigravity processes, discovery, login/OAuth, usage polling, OIDC, or telemetry relay.

## `[server.rate_limits]`

| Table | `max` default | `window_seconds` default | Meaning |
| :-- | :-- | :-- | :-- |
| `device_authorization` | `30` | `600` | Per-IP fixed-window limit for `POST /oauth/device_authorization` |
| `device_verify` | `10` | `600` | Independent per-IP fixed-window limit for `user_code` submissions at `POST /device` |

Both `max` and `window_seconds` must be greater than zero. These tables are inert without `[server.gateway]`. The limiter stores are created at boot, so changes require a restart.

## `[server.auth]` (optional)

Presence of this table enables inbound client-token auth ([details](/guides/shared-gateway/)):

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `header` | `x-shunt-token` | Header carrying the client token |
| `tokens_env` | `SHUNT_CLIENT_TOKENS` | Env var holding comma-separated `name:token` pairs |

The named environment variable must contain one or more credentials, for example `SHUNT_CLIENT_TOKENS="alice:<token>,bob:<token>"`. Startup fails closed if the table is present but the variable is unset, empty, or malformed. Gated routes (mapped `/v1/messages` inference and `GET /v1/models` discovery) accept the token via the configured header, `Authorization: Bearer`, or `x-api-key` — the dedicated header wins when several carry valid tokens.

`tokens_env`'s own value, like any config-file string, can also be written as `${VAR}` / `${file:...}` (see [Secret references](#secret-references)); it still names the environment variable shunt reads the tokens from.

A request whose whole route chain is `passthrough` skips this gate: such a route forwards the caller's own upstream credential, so the gateway lends them nothing. **`kind = "antigravity_cli"` is exempt from that exemption** and is always treated as credential-injecting, whatever its `auth` says. The adapter ignores the caller's credential entirely and runs the operator's local `agy` with `--dangerously-skip-permissions`, so a passthrough Antigravity route would otherwise be unauthenticated local code execution as the user running shunt — sandboxed or not. Note this closes the exemption, it does not create a requirement: with neither `[server.auth]` nor [`[server.gateway]`](#servergateway-optional) configured, every route stays open, which is why an unsandboxed Antigravity provider is [refused outright off loopback](#providersname-legacy).

## `[server.admin]` (optional)

Presence of this table enables the admin web surface for browser account provisioning and account-pool health ([details](/guides/admin-remote-provisioning/)). When the table is absent, none of the `/admin*` routes are registered. The same credential also authenticates the [`[server.spend]`](#serverspend-optional) spend-limit API.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `header` | `x-shunt-admin-token` | Header carrying the admin credential for API/curl calls. `x-api-key` is accepted alongside it on the admin and spend-limit routers |
| `tokens_env` | `SHUNT_ADMIN_TOKENS` | Env var holding comma-separated `name:token` pairs. These are the **write** tier |
| `tokens_file` | _(unset)_ | Path to a file holding `name:token` pairs (one per line, or comma-separated), used when `tokens_env` is unset/empty. Also the **write** tier |
| `session_ttl_secs` | `3600` | Browser session lifetime after login, in seconds |
| `pending_ttl_secs` | `600` | Time allowed to finish a started provisioning flow, in seconds |
| `hide_observed` | `false` | When `true`, shunt does not read host CLI/app logins at all. `GET /admin/api/observed` still requires admin auth but returns an empty list, and the dashboard's **Accounts and usage** table lists managed pool accounts only. Hot-applies on config reload |

Admin tokens can come from the environment or a file. The named environment variable must contain one or more credentials, for example `SHUNT_ADMIN_TOKENS="ops:<token>"`. Alternatively, set `tokens_file` to a path (`~` is expanded) and put the pairs there — this is what [`shunt dashboard setup`](/reference/cli/#shunt-dashboard-setup) writes to `~/.shunt/admin-token`, so no secret has to live in the launch environment. When both are set, a non-empty `tokens_env` wins.

As with `[server.auth]` above, `tokens_env`'s and `tokens_file`'s own values can also be written as `${VAR}` / `${file:...}` (see [Secret references](#secret-references)).

Admin credentials are separate credentials from the client tokens configured under `[server.auth]`; do not reuse one credential for both surfaces. An admin credential authenticates only the `/admin*` and spend-limit routes — never an inference route, where `x-api-key` is the caller's own Anthropic credential slot. Whatever these routers accept in a slot is also stripped from that slot before any upstream request, so an admin credential is never relayed to a provider.

### `[[server.admin.write_keys]]` / `[[server.admin.read_keys]]` (optional)

Two arrays of per-credential keys, each entry a `{ id, key }` table. The `id` is safe to log and is what the spend-limit audit trail records as `admin-key:<id>`; a `tokens_env`/`tokens_file` pair is recorded as `admin-token:<name>` instead.

```toml
[[server.admin.write_keys]]
id = "terraform"
key = "${SHUNT_ADMIN_KEY_TERRAFORM}"

[[server.admin.read_keys]]
id = "reporting"
key = "${file:/run/secrets/shunt-reporting-key}"
```

| Array | Access | Meaning |
| :-- | :-- | :-- |
| `write_keys` | `write` | Full access: `write` implies `read`. The same tier as `tokens_env`/`tokens_file` |
| `read_keys` | `read` | Passes every `GET` on the admin surface and the spend-limit API; refused with `403 permission_error` on every mutation. It signs in to the dashboard as a read-only session: `POST /admin/login` accepts it, the session records the `read` tier, and every mutation behind that cookie is still refused with `403` |

A credential's privilege is the maximum over every set it matches, so the order the sets are scanned in cannot change it. Each `id` must be non-blank and each key at least 32 characters; ids **and** key values must each be unique across all three credential sets (`tokens_env`/`tokens_file`, `write_keys`, `read_keys`), and a collision names the colliding ids without logging a key value. A legacy `tokens_env` token shorter than 32 characters warns rather than failing, because those tokens predate the rule.

Each `key` is a redacting secret (see [Secret references](#secret-references)) and is the one field where a literal **fails config load** instead of warning: supply it with `${VAR}`, `${file:/abs/path}`, or a `SHUNT_*` environment override.

Startup fails closed if `[server.admin]` is present but all three credential sources are empty (unset/empty/malformed `tokens_env`, an unreadable `tokens_file`, and no array entries). An array-only deployment, with `tokens_env` unset, boots.

### `[server.admin.oidc]` (optional)

Presence of this subtable adds an OIDC/SSO button to the admin browser login page. Admin tokens remain mandatory for API/curl access and as the token-form fallback. The allowlist is matched case-insensitively.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `public_url` | required | Externally reachable bare HTTPS origin for the admin surface; loopback HTTP is allowed. The redirect URI is `{public_url}/admin/oidc/callback` |
| `issuer` | required | OIDC discovery issuer. Must use HTTPS, except HTTP on `localhost` or `127.0.0.1`; a path is allowed |
| `client_id` | required | OIDC client id |
| `client_secret_env` | `SHUNT_ADMIN_OIDC_SECRET` | Env var holding the non-empty client secret |
| `allowed_domains` | `[]` | Case-insensitive email domains allowed to administer shunt |
| `allowed_emails` | `[]` | Case-insensitive full email addresses allowed to administer shunt |
| `scopes` | `openid email profile` | Scopes sent to the authorization endpoint |
| `authorization_endpoint` | discovery | Advanced authorization URL override; HTTPS, or HTTP on `localhost`/`127.0.0.1` only |
| `token_endpoint` | discovery | Advanced token URL override; HTTPS, or HTTP on `localhost`/`127.0.0.1` only |
| `userinfo_endpoint` | discovery | Advanced OIDC UserInfo URL override; HTTPS, or HTTP on `localhost`/`127.0.0.1` only |

At least one non-empty `allowed_domains` or `allowed_emails` entry is mandatory. Startup also fails closed for an invalid `public_url`, empty issuer/client id, or missing client secret. shunt accepts only a non-empty UserInfo email with `email_verified = true`. The browser flow uses PKCE and a `pending_ttl_secs`-bound, single-use state; callback/token/UserInfo failures produce generic browser messages without echoing provider input. The callback re-checks the current hot-reloaded allowlist before minting the same HttpOnly admin session cookie as token login, then redirects to the fixed `/admin` target.

`client_secret_env`'s own value can also be written as `${VAR}` / `${file:...}` (see [Secret references](#secret-references)).

For GitHub, SAML, or another non-OIDC provider, use an OIDC broker such as Dex; direct provider-specific OAuth2 integrations are out of scope.

## `[server.spend]` (optional)

Presence of this table registers the spend-limit Admin API under `/v1/organizations/spend_limits`. It is a top-level section holding **policy only** — no key material: the routes authenticate with the [`[server.admin]`](#serveradmin-optional) credential, so enabling spend limits does not require the gateway login surface. `[server.spend]` without `[server.admin]` fails configuration validation.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `blocked_message` | unset | Accepted for future enforcement errors; stage 1 does not use it |
| `audit_retention_days` | `365` | Accepted for the later audit retention sweep |
| `spend_retention_months` | `13` | Accepted for the later spend retention sweep |
| `identity_retention_days` | `90` | Accepted for the later identity retention sweep |
| `group_limit_mode` | `min` | `min` or `max`; accepted for later group-limit resolution |
| `state_path` | `~/.shunt/gateway-spend.json` | Versioned JSON file containing caps and audit records; `""` selects memory-only storage |

Send the admin credential in the configured `[server.admin] header` or in `x-api-key`; a `read_keys` credential can use `GET` only. The state file is replaced atomically with private permissions after each mutation. When no home directory resolves, the default is memory-only. Adding or removing the table, and the state path itself, are both fixed at boot; configuration reloads log a warning instead of applying them.

### `[server.spend.enforcement]` (optional)

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `fail_closed_on_error` | `false` | Accepted for the later enforcement stage; stage 1 does not read it |

Stage 1 does not enforce caps on `/v1/messages`. It also does not implement usage metering, `/effective`, `/audit`, retention sweeps, or group scopes.

## `[server.gateway]` (optional)

Presence of this table enables the [OAuth device-flow gateway login](/guides/gateway-login/) used by Claude Code's managed `forceLoginMethod: "gateway"`. When absent, shunt does not register `/.well-known/oauth-authorization-server`, `/oauth/device_authorization`, `/oauth/token`, `/device`, `/device/authorize`, `/device/callback`, `/managed/settings`, or the `POST /v1/metrics`, `POST /v1/logs`, and `POST /v1/traces` telemetry-ingest routes.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `public_url` | required | Externally reachable HTTPS origin used as the JWT issuer and base for advertised OAuth endpoints; `http` is accepted only for loopback |
| `jwt_secret_env` | `SHUNT_GATEWAY_JWT_SECRET` | Env var holding the HS256 signing secret (at least 32 bytes). **Deprecated**, still fully supported — superseded by [`session.jwt_secret`](#servergatewaysession-optional) |
| `users_env` | `SHUNT_GATEWAY_USERS` | Env var holding comma-separated `email:secret` approval users; optional when `[server.gateway.oidc]` is configured |
| `token_ttl_seconds` | `3600` | Access-token lifetime; returned as `expires_in`. **Deprecated**, still fully supported — superseded by [`session.ttl_hours`](#servergatewaysession-optional), except that this key remains the only way to express a sub-hour lifetime |
| `trust_forwarded_for` | `false` | Trust `X-Forwarded-For`/`X-Real-IP` as the `/device` rate-limit identity; enable only behind a trusted proxy that replaces client-supplied values |
| `state_path` | `~/.shunt/gateway-sessions.json` | File persisting refresh sessions across restarts; tokens are stored as SHA-256 hashes and written atomically with owner-only permissions (0600 on Unix). Set `""` for memory-only sessions (also the fallback when no home directory resolves) |

Startup fails closed when the URL is not a bare HTTPS origin (`http` is allowed only on loopback), the TTL is zero, the secret is missing or shorter than 32 bytes, or neither a valid static-user list nor a valid external IdP is configured. Static-user secrets may contain `:` because only the first colon separates the email and secret. Changes to the environment-backed secrets, users, and IdP configuration hot-apply on config reload; adding or removing the gateway table requires a restart because the route tree is fixed at boot.

`jwt_secret_env`'s and `users_env`'s own values can also be written as `${VAR}` / `${file:...}` (see [Secret references](#secret-references)).

Setting both a deprecated key and its `[server.gateway.session]` replacement fails startup, per key: `jwt_secret_env` together with `session.jwt_secret` is an error, and `token_ttl_seconds` together with `session.ttl_hours` is an error; mixing across the two pairs (e.g. `token_ttl_seconds` alongside `session.jwt_secret`) is fine. shunt logs one deprecation warning whenever a deprecated key is explicitly set — whether in the config file or through a `SHUNT_*` environment override — and stays silent only when the key itself is never configured and its default applies; a config that never sets `jwt_secret_env` and simply relies on the `SHUNT_GATEWAY_JWT_SECRET` env var holding the secret still doesn't warn, since that variable holds the secret's value, not the deprecated key being set. Where only one side of a pair is set, `session.*` wins if present, else the deprecated key, else the default.

### `[server.gateway.session]` (optional)

Mirrors the upstream Claude apps gateway `session:` block:

```toml
[server.gateway.session]
jwt_secret = "${SHUNT_GATEWAY_JWT_SECRET}"
ttl_hours = 1
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `jwt_secret` | required when this table is present | HS256 signing secret, at least 32 bytes of entropy (e.g. `openssl rand -base64 32`). A single string, or an array for rotation — index 0 signs new tokens and every entry verifies |
| `ttl_hours` | `1` | Access-token lifetime, in whole hours |

`jwt_secret` is a `Secret`-typed field: its value supports `${VAR}` / `${file:/abs/path}` like any other config string (see [Secret references](#secret-references)) and is redacted in diagnostic output. To rotate without invalidating live sessions, prepend the new secret to the array, wait `ttl_hours` for outstanding access tokens to expire, then drop the old entry:

```toml
[server.gateway.session]
jwt_secret = ["new-secret-value", "old-secret-value"]
```

### `[server.gateway.oidc]` (optional)

Presence of this subtable replaces or supplements the password approval form with an OIDC provider such as Google. An allowlist is always required and is matched case-insensitively.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `issuer` | required | OIDC discovery issuer. Must use HTTPS, except HTTP on `localhost` or `127.0.0.1`; a path is allowed |
| `client_id` | required | OIDC client id |
| `client_secret_env` | `SHUNT_GATEWAY_OIDC_SECRET` | Env var holding the non-empty client secret |
| `allowed_domains` | `[]` | Case-insensitive email domains allowed to approve a device |
| `allowed_emails` | `[]` | Case-insensitive full email addresses allowed to approve a device |
| `scopes` | `openid email profile` | Scopes sent to the authorization endpoint; custom values must include `openid` and `email` |
| `authorization_endpoint` | discovery | Advanced authorization URL override; HTTPS, or HTTP on `localhost`/`127.0.0.1` only |
| `token_endpoint` | discovery | Advanced token URL override; HTTPS, or HTTP on `localhost`/`127.0.0.1` only |
| `userinfo_endpoint` | discovery | Advanced OIDC UserInfo URL override; HTTPS, or HTTP on `localhost`/`127.0.0.1` only |

At least one non-empty `allowed_domains` or `allowed_emails` entry is mandatory. shunt accepts only a non-empty UserInfo email with `email_verified = true`. The browser flow uses a single-use ten-minute state and PKCE, and callback/token/UserInfo failures produce generic browser messages without echoing provider input. The redirect URI registered at the provider is `{public_url}/device/callback`. For GitHub, SAML, or another non-OIDC provider, use an OIDC broker such as Dex; direct provider-specific OAuth2 integrations are out of scope.

`client_secret_env`'s own value can also be written as `${VAR}` / `${file:...}` (see [Secret references](#secret-references)).

The issued bearer gates `/v1/models` and `/v1/messages`/`/v1/messages/count_tokens` requests whenever the selected provider injects a server-side credential; passthrough providers remain open. If `[server.auth]` is also present, either credential grants access. Refresh sessions persist across restarts by default: `state_path` (tokens hashed at rest) is restored at boot, so users keep silently refreshing. The file must not be shared between concurrent shunt processes. With `state_path = ""`, sessions are memory-only — a config reload preserves them, but restarting shunt invalidates them and users sign in again once their access JWT expires. Device grants and rate-limit counters are always memory-only; a restart mid-login only costs that attempt. Expired grants and idle rate-limit identities are swept opportunistically. Device grants and rate-limit identities are each capped at 4,096 entries. Used refresh-token tombstones are retained for 30 days and capped at 64 per family; active refresh tokens idle for 30 days expire.

### `[[server.gateway.policies]]` (optional)

Presence of `[server.gateway]` registers authenticated `GET /managed/settings`; an ordered, non-empty policy list supplies its managed document. Each policy has an optional `[server.gateway.policies.match]` table and a required open-schema `[server.gateway.policies.cli]` object. `match` omitted, `match = {}`, or no `emails` means catch-all; an explicit empty `emails` list or blank entry fails startup.

All catch-all policies merge in order, then the first exact, case-sensitive email match merges on top. Objects merge recursively; arrays replace except keys containing `deny`, whose arrays union without duplicates. Known keys are validated at startup and hot reload: `availableModels`, when present, must be an array containing only strings, and `env`, when present, must be a table containing only scalar string, number, or boolean values. Unknown keys remain open-schema, but every value must be JSON-representable; non-finite floats are rejected.

No `policies` key makes the endpoint return `404`. With policies configured but no matching user-specific or catch-all settings, it returns `200` with a telemetry-only `settings.env` when telemetry is enabled, and `settings: {}` otherwise. Responses carry `uuid`, `checksum`, and a quoted `ETag` containing the checksum; matching `If-None-Match` returns `304`.

If the resolved `cli.availableModels` is an array of strings, gateway-JWT requests to `/v1/messages` and `/v1/messages/count_tokens` are rejected with `400 invalid_request_error` when their top-level `model`, after stripping one trailing Claude Code context-window hint (`[1m]` or `[1M]`), is absent from the list. Static `[server.auth]` credentials remain unrestricted because they do not identify a gateway policy user.

### `[server.gateway.telemetry]` (optional)

`forward_to` is an array of destinations, each with a required base OTLP/HTTP `url`, an optional string `headers` map, and per-signal opt-in booleans.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `url` | required | Base OTLP/HTTP endpoint: scheme, host, and optional path, `http(s)` only. A query string, fragment, or embedded userinfo is rejected at startup. shunt trims a trailing `/` and appends `/v1/metrics`, `/v1/logs`, or `/v1/traces` |
| `headers` | none | Extra request headers applied to every relay to this destination; a configured key replaces the forwarded value rather than duplicating the header. Names and values are validated at startup. Each header value is a redacting secret and renders as `[redacted]` in diagnostic output; see [Secret references](#secret-references) |
| `metrics` | `true` | Relay `POST /v1/metrics` to this destination |
| `logs` | `false` | Relay `POST /v1/logs` to this destination |
| `traces` | `false` | Relay `POST /v1/traces` to this destination |

A list with at least one opted-in signal injects six values into managed `settings.env`: `CLAUDE_CODE_ENABLE_TELEMETRY=1`, each of `OTEL_METRICS_EXPORTER`, `OTEL_LOGS_EXPORTER`, and `OTEL_TRACES_EXPORTER` set to `otlp` when some destination opts in to that signal and `none` otherwise, `OTEL_EXPORTER_OTLP_ENDPOINT` set to `public_url`, and `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`. A signal the gateway would only discard is disabled on the client instead of uploaded; when no destination opts in to any signal, nothing is injected. Policy env values win on conflicts.

The same list also drives inbound ingest: the `POST /v1/metrics`, `POST /v1/logs`, and `POST /v1/traces` routes — registered whenever `[server.gateway]` is present — accept the OTLP payloads those clients export and relay them verbatim to every destination that opted in to the signal; a signal with no opted-in destination is accepted and discarded. At most 64 relays run in flight at once; beyond that a payload is shed rather than queued, so a saturated gateway never adds client-visible latency. Logs and traces default off because Claude Code log records and spans can carry command lines, prompts, and file paths. See the [gateway login guide](/guides/gateway-login/#telemetry-ingest).

```toml
[[server.gateway.policies]]
[server.gateway.policies.match]
emails = ["alice@example.com"]
[server.gateway.policies.cli]
availableModels = ["claude-opus-4-8"]
[server.gateway.policies.cli.env]
DISABLE_UPDATES = "1"

[server.gateway.telemetry]
[[server.gateway.telemetry.forward_to]]
url = "https://collector.example.com"
logs = true
headers = { "x-api-key" = "..." }
```

By default, `/device` ignores forwarding headers and rate-limits the socket peer. Set `trust_forwarded_for = true` only when shunt is reachable exclusively through a trusted reverse proxy that removes client-provided forwarding headers before setting its own value. Do not enable it on a directly exposed gateway.

## `[server.codex_endpoint]` (optional)

Presence of this table enables an inbound OpenAI Responses passthrough so the **Codex CLI** can point its `base_url` at shunt and be load-balanced across a ChatGPT/Codex OAuth account pool ([details](/guides/inbound-codex-endpoint/)). When the table is absent, none of those routes are registered.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `provider` | `codex` | Configured upstream name to serve inbound requests whose `model` matches no route; must use `auth = "chatgpt_oauth"` |
| `routes` | `[]` | Opt-in per-model routing (see below) |

Registers `POST /backend-api/codex/responses`, `POST /responses`, and `POST /v1/responses` — all served by the named provider's account pool. When `[server.auth]` is configured they require a valid client token (like the other injected-credential routes); with no `[server.auth]` they are **open** to anyone who can reach them while still injecting the operator's Codex credential, so gate them on anything beyond loopback. Unlike `/v1/messages`, the request is not translated to or from Anthropic Messages; it is relayed to and from the upstream verbatim.

### `[[server.codex_endpoint.routes]]` (optional)

Each entry sends one model to a different Responses-compatible upstream, instead of the fixed `provider` above.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `model` | *(required)* | Public model id the Codex client sends in the Responses body. Matched **exactly** and **case-sensitively** — no prefix match, no `[1m]` stripping, and no charset restriction, so vendor slugs like `MiniMax-M3`, `openai/gpt-5.6-sol`, and `~openai/gpt-latest` route as written |
| `provider` | *(required)* | Configured provider that serves this model; must be `kind = "responses"` and must not use a credential-free auth mode (`passthrough` or `none`) |
| `upstream_model` | `model` | Model id sent upstream. When it differs from `model`, shunt rewrites the body's top-level `model` and leaves every other field intact |

Validation rejects a route to an unknown provider, to a non-`responses` provider, to a provider whose auth mode carries no credential (`passthrough` or `none`), and rejects duplicate `model` entries or blank fields. Routes are read from the live config snapshot, so adding, editing, or removing one takes effect on **reload**; only toggling the `[server.codex_endpoint]` table itself needs a restart. A routed request to a non-ChatGPT provider sends a fresh header allowlist (`content-type`, `accept`, plus the flavor-gated `OpenAI-Beta` and, for an `xai_oauth` route, the Grok-CLI identity headers), an identity-encoded body, and one credential with no pool or failover.
The same opt-in registers `GET /models` and `GET /backend-api/codex/models`, which return the valid Codex fallback `{"models":[]}` after the normal model-discovery auth gate. It also enables Codex negotiation on the shared `GET /v1/models`: when `client_version` is present in the query, that field takes precedence over Anthropic-like headers and selects the Codex empty shape. Without `client_version`, the existing Anthropic discovery response is unchanged. shunt intentionally does not synthesize incomplete Codex `ModelInfo` rows.

## `[server.usage]` (optional)

Presence of this table registers a client-facing `GET /usage` endpoint that returns a **sanitized, aggregated** view of the shared account pool's quota state, so a non-admin client can anticipate throttling without the admin surface ([endpoint details](/reference/endpoints/)). When the table is absent, the route is not registered.

The table has no keys today — presence alone opts in. It **requires [`[server.auth]`](#serverauth-optional)**: the endpoint identifies its caller by client token, so shunt fails startup if `[server.usage]` is set without inbound auth rather than serve pool telemetry unauthenticated.

`GET /usage` authenticates the same client token as `/v1/messages` (configured header, `x-api-key`, or `Authorization: Bearer`) and reports per-window remaining headroom (`mean(1 - utilization)` across non-disabled accounts that report the window, i.e. the fraction of the pool's combined capacity still unused — nine exhausted accounts plus one fresh one read `0.1` — a pool-wide aggregate, not a prediction of whether the next request will be admitted), each window's reset, and a coarse `ok`/`degraded`/`exhausted` status. It never exposes account names, counts, priorities, `disabled` flags, thresholds, or per-account numbers — the full per-account detail stays behind the admin-only [`GET /admin/api/pool`](#serveradmin-optional). A window is `null` only when no non-disabled account reports it. Codex response `x-codex-*` headers and optional `wham/usage` polling populate the observed 5-hour and shared weekly windows; an unobserved window alone is `null`. Codex has no Fable-scoped (`7d_oi`) signal, although another provider in a mixed pool may supply the aggregate Fable window.

The response carries the pool-wide aggregate under `pool` and, under `providers`, the same sanitized aggregate computed per pooled provider, keyed by the configured provider name (the `<name>` in `[providers.<name>]` or the `name` of a `[[upstreams]]` entry — not an account identity). Mapping a model to that key is the client's job: [`GET /routes`](/reference/endpoints/) covers only models explicitly listed in `[[routes]]`, no endpoint exposes the `[[models]].upstream_model`, `[[route_prefixes]]`, or `server.default_provider` mappings, and `GET /v1/models` entries carry no provider field. On a mixed pool, `pool` blends every provider's accounts into one mean, so a client that routes to one provider should read `providers.<name>` for that provider's headroom and status. Providers whose auth mode is not pooled are omitted, and a provider's `fable` window is `null` when that provider has no Fable-scoped signal even if `pool` reports one. See the [endpoint reference](/reference/endpoints/) for the full shape.

## `[server.oauth_usage]` (optional)

Presence of this table registers `GET /api/oauth/usage` — the exact path Claude Code CLI's own native usage bars fetch — so the CLI's unmodified UI can show real pool numbers when pointed at shunt via `ANTHROPIC_BASE_URL` ([endpoint details](/reference/endpoints/), [M14 behavior specification](https://github.com/pleaseai/shunt/blob/main/docs/m14-oauth-usage-endpoint.md)). When the table is absent, the route is not registered.

The table has no keys today — presence alone opts in. Its auth model differs from `[server.usage]`: on a loopback `[server.bind]` the route is unauthenticated (the caller cannot have reached it off the operator's own machine); on a non-loopback bind it requires a **valid** credential — a configured client token or a valid gateway JWT, gated exactly as `/v1/messages` is (bare header presence is not accepted) — and shunt fails startup (`OauthUsageEndpointRequiresAuthOnNonLoopback`) unless [`[server.auth]`](#serverauth-optional) or [`[server.gateway]`](#servergateway-optional) is also configured. shunt also refuses to boot if a `claude_oauth` provider's `base_url` resolves to this gateway's own bind (`OauthUsageSelfPollLoop`) — otherwise the outbound usage poller could read back its own synthesized aggregate instead of Anthropic's real usage.

`GET /api/oauth/usage` reports only `claude_oauth`-provider accounts (never Codex/Cursor/Grok), and uses a routing-aware, priority-tiered worst case per window rather than `/usage`'s pool-wide mean headroom: within the lowest-`priority` tier of available accounts (falling back to the full non-disabled set when none are available), it reports the *maximum* utilization — the worst case among the accounts the next request can actually route to, not a pool-wide average. **This route only helps when the CLI itself is configured to call it**, which was verified to happen only for a full interactive `claude login` session, not `claude setup-token` or a shared-gateway client token — see the design note for the full precondition evidence.

## `[server.pool]` (optional)

Quota-aware load-balancing tuning for the account pools — Claude (Anthropic) ([details](/guides/anthropic-multi-account/#tuning-selection-serverpool)) and, since issue #195, Codex/ChatGPT ([details](/guides/codex-multi-account/)). When the table is absent, selection uses the single built-in `0.98` threshold exactly as before this table existed.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `hard_threshold` | `0.98` | Safety backstop for every quota window; an account at or above it always sorts last among available accounts |
| `default_threshold` | unset | Soft default threshold for any window without a more specific value |
| `default_threshold_5h` | unset | Soft default for the 5-hour window |
| `default_threshold_7d` | unset | Soft default for the shared weekly (`7d`) window |
| `default_threshold_fable` | unset | Soft default for the fable-only weekly (`7d_oi`) window |
| `burn_rate_avoidance` | `false` | Also avoid accounts projected to exhaust a window's soft threshold before that window resets |
| `usage_refresh_seconds` | disabled (`0`/absent) | Poll interval, in seconds, for Claude `GET /api/oauth/usage` and Codex `GET /wham/usage`; a positive value below 60 is clamped up to a 60-second floor |
| `state_path` | unset | File the pool's per-account quota state is persisted to, so a restart warm-starts from the last observed utilization instead of an empty pool. Absent disables persistence (the default) |
| `ramp_initial_concurrency` | disabled (`0`/absent) | Storm control: initial concurrent-admission allowance for an account identity that just started taking traffic. `0` or absent disables admission gating |
| `reprobe_seconds` | `900` once this table is present; `0` disables | Opportunistic-reprobe interval, in seconds, for a stale near-quota Codex/ChatGPT account; a positive value below 60 is clamped up to a 60-second floor. `0` disables re-probing; when `[server.pool]` itself is absent, re-probing is disabled regardless of this value (pre-issue-#135 behavior). Non-WebSocket outbound Responses selection and the optional inbound Codex HTTP endpoint retain re-probing; WebSocket-enabled outbound selection disables it |

For each window `X`, the effective soft threshold resolves as: account `threshold_X` → account `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`, and is capped at `hard_threshold`. All thresholds are utilization fractions in `[0.0, 1.0]`; out-of-range values fail startup. The threshold and burn-rate knobs govern both pool families: the Anthropic pool from its `anthropic-ratelimit-unified-*` headers, and the Codex/ChatGPT pool from its `x-codex-*` 5-hour/weekly windows (Codex has no Fable-scoped `7d_oi` window, so `default_threshold_fable` is inert there). `usage_refresh_seconds` polls both families: Anthropic accounts use the official API, while imported Codex/ChatGPT-backend accounts use the private, unofficial `wham/usage` endpoint ([details](/guides/codex-multi-account/#usage-poller)).

A positive `usage_refresh_seconds` starts a background poller that reconciles account-pool quota state against each family's usage API ([Anthropic details](/guides/anthropic-multi-account/#usage-api-reconciliation), [Codex details](/guides/codex-multi-account/#usage-poller)); absent or `0` disables it (the default). Only imported (refreshable) accounts of either family are polled — a long-lived `claude setup-token`, or a `token_env` account of either family, is skipped because the usage endpoint rejects a non-refreshable token. For Claude, the poller reconciles each reported window's utilization, its own reset time, and its utilization observation time; only per-window and aggregate status freshness and the reset boundary captured when a status was observed remain header-derived, including when authoritative usage contains out-of-band consumption of the same account outside shunt. For Codex, it reconciles utilization and utilization observation time while keeping reset metadata response-derived (the `x-codex-*` headers and the WebSocket `codex.rate_limits` event) and status metadata header-derived. For a reported window, a future stored reset survives, while an elapsed stored reset is cleared before fresh utilization is written; the parsed wham `reset_at` is not adopted as live reset metadata. For Codex, the private endpoint's schema is observed rather than documented, so parsing is lenient and fail-soft. The interval is fixed at boot; a config reload does not start, stop, or re-tune the poller.

`state_path` persists the pool's quota state (per-window utilization and each window's own reset, independent utilization/status observation times and status reset boundaries, across every provider's accounts) to disk. Without it, a restart begins with an empty pool: every account looks unseen until its first post-restart response, which disables burn-rate avoidance and leaves `GET /usage` blank until traffic re-populates the pool. The file is a best-effort cache, not a source of truth — quota is re-derived from upstream responses regardless, so a missing, stale, or corrupt file only costs a cold start, never a boot failure. Writes use a private (`0600` on Unix) temp file, atomically rename it over the target, and happen on a background timer only when quota changed; failed writes retry on the next tick. Cooldowns are not persisted (they lapse on restart), and any restored window whose reset has already passed is dropped during import, before the first selection or snapshot after restore. A reset-less utilization or status signal expires one window length after its own observation time. Version-2 files migrate through an explicit legacy path and are rewritten as version 3. An aggregate `status` without `observed_at_status` captures the earliest persisted `reset_5h`, `reset_7d`, or `reset_7d_oi` as an immutable deadline. If that reset has already passed, the expired reset, the unstamped aggregate status, and its synthesized stamp are removed during the same import. A reset beyond the plausible seven-day horizon is conservatively bounded at boot plus seven days; an aggregate with no reset starts its seven-day cap at boot. Existing v2 stamps are not reinterpreted from reset metadata, while normal import still normalizes orphan metadata, expires elapsed signals, clamps future timestamps to boot, and supplies boot time to a surviving unstamped aggregate when appropriate. Later reset-only or usage updates cannot extend the captured deadline, and the result remains equivalent after the v3 rewrite and a second restore. A version-3 reset-less status stays reset-less after reset-only updates. The path is fixed at boot; a config reload does not start, stop, or re-point persistence.

A positive `ramp_initial_concurrency` enables **storm control** on every account pool: after a failover switch, concurrent in-flight requests would otherwise all land on the freshly selected account at once. With the gate on, an identity that just started taking traffic (fresh, back from a cooldown, or idle for 60 seconds) admits at most the configured number of concurrent requests; each successful response doubles the allowance (slow start), a failover-worthy failure restarts the ramp, and a denied request spills to the next account in selection order. The last remaining candidate is always attempted regardless of the gate, so gating can defer but never fail a request that an ungated pool would have served. Note this also means a pool whose accounts all resolve to a single upstream identity is effectively ungated: its only candidate is always the last candidate, so the setting only takes effect with two or more distinct account identities.

`reprobe_seconds` is a safety net for a Codex/ChatGPT pool while out-of-band usage polling is unavailable or between polls: a rotation-representative account that is Codex/ChatGPT-family, near quota, off cooldown, and whose freshest observation is older than the interval gets promoted to the front of selection and reserved once per interval. Freshness is the newest of four logical values: the newer of utilization and 5h status observation, the newer of utilization and shared 7d status observation, the newer of utilization and Fable 7d_oi status observation, and the independent aggregate status observation. A usage-only poll updates utilization freshness but never per-window status freshness. The next live request can then refresh the quota instead of leaving the account excluded until its far-future weekly reset. Admission or credential-resolution failure cancels the reservation; the first actual HTTP dispatch commits the probe timestamp and `shunt.pool.reprobes`. Only Codex/ChatGPT accounts are eligible — Claude and Kimi use a slower cooldown recovery (`PauseSame`, up to five minutes) on a generic rejection, so an opportunistic probe there risks stalling a real request; Claude accounts have `usage_refresh_seconds` above instead. A configured poller provides early recovery only for imported, refreshable `chatgpt_oauth` accounts; without it, or for an ineligible account, an excluded outbound mark clears only at its observation-time window bound. Re-probing costs one live request's worth of upstream traffic per promotion, unlike `usage_refresh_seconds`'s out-of-band metadata poll. When WebSocket transport is enabled for a provider, the outbound Responses pool creates no reservation and suppresses re-probing; the optional inbound Codex HTTP endpoint still probes, and `shunt.pool.reprobes` for that provider counts inbound probes only.

## `[server.status]` (optional)

Observation-only background polling of provider Statuspage `summary.json` endpoints, for visibility rather than decisioning: it never feeds routing, failover, or pool/cooldown behavior. It only updates a shared store surfaced by the `shunt.upstream.status` metric and the admin dashboard's "Upstream status" strip ([`GET /admin/api/status`](/reference/endpoints/)). When the table is absent, or `sources` is empty, the poller does not start.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `refresh_seconds` | `300` | Poll interval, in seconds; a positive value below 60 is clamped up to a 60-second floor. `0` disables polling |
| `sources` | `[]` | Array of `{ provider, url }` tables, one per Statuspage `summary.json` endpoint to poll |

```toml
[server.status]
refresh_seconds = 300

[[server.status.sources]]
provider = "claude"
url = "https://status.claude.com/api/v2/summary.json"

[[server.status.sources]]
provider = "openai"
url = "https://status.openai.com/api/v2/summary.json"
```

Each `sources` entry needs a non-empty, unique `provider` label and an `http`/`https` `url` without a query, fragment, or embedded credentials. Shunt fails startup on an empty or duplicate `provider`, or an invalid URL shape. This validation is fail-closed (a bad config refuses to boot), unlike the poller's own runtime behavior below, which is deliberately fail-open into an explicit "no signal" state.

A fetch failure, non-2xx response, oversized body (capped at 1 MiB), invalid JSON, or an unrecognized `indicator` string in the response all resolve to `unknown` ("no signal") rather than `none` ("operational"): a failed poll can only ever replace a source's stored entry with `unknown`, never leave a stale "operational" value in place or report a false all-clear for a source shunt could not actually reach. Sources in the `unknown` state are also omitted from the `shunt.upstream.status` metric entirely, rather than reported as a `0` sample.

`GET /admin/api/status` (admin-authenticated) returns each configured source's most recently observed indicator, description, incidents, and observed timestamp. A configured source whose first poll has not completed is returned as `unknown`; an unconfigured or empty `[server.status]` reports an empty `sources` list, which the dashboard reads as "hide this section" rather than rendering an empty table.

Whether the poller runs at all, and its polling interval, are decided once from the boot config — exactly like `[server.pool] usage_refresh_seconds` above: if `[server.status]` is absent, empty, or `refresh_seconds` is `0` at boot, no background task is created, and a later reload that enables it does not retroactively start one. Once running, each tick re-reads the current `sources` list from the live (possibly reloaded) config, so edits to which sources are polled take effect from the next tick onward; the polling interval itself does not change on reload.

## `[[upstreams]]` (ordered failover)

`[[upstreams]]` is an ordered array of named upstreams. Declaration order is the global failover order; a model's `[models.upstream_model]` map selects which entries participate. The map's textual order does not affect routing.

```toml
[server]
default_provider = "anthropic-primary"

[[upstreams]]
name = "anthropic-primary"
provider = "anthropic"
auth = { mode = "claude_oauth", account = "primary" }

[[upstreams]]
name = "kimi-overflow"
provider = "kimi"

[[upstreams]]
name = "codex-fallback"
provider = "codex"

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic-primary = "claude-opus-4-8"
kimi-overflow = "kimi-k2"
codex-fallback = "gpt-5.2"
```

The example attempts `anthropic-primary`, `kimi-overflow`, and `codex-fallback`, in that order. An upstream omitted from the model map does not participate.

| Key | Required | Meaning |
| :-- | :-- | :-- |
| `name` | yes | Unique non-empty upstream name. Routes, model maps, `server.default_provider`, metrics, and admin views use this name. |
| `provider` | unless `kind` + `base_url` are set | Built-in preset. Supplies `kind`, `base_url`, and default auth. Explicit fields override preset values. |
| `kind` | without a preset | `anthropic`, `responses`, `cursor`, `gemini`, `antigravity`, or `antigravity_cli`. The last three have no entry in the preset table below — the built-in `[providers.gemini]`, `[providers.antigravity]`, and `[providers.antigravity-cli]` tables are the separate legacy mechanism, not presets — so an ordered upstream on any of the three must set `kind` explicitly. Note that the CLI provider's table is named `antigravity-cli` with a hyphen, while its `kind` value is `antigravity_cli` with an underscore. |
| `base_url` | without a preset | Upstream base URL. For `kind = "cursor"`, this is the login/token-refresh surface only; inference uses the fixed agent host `https://agentn.global.api5.cursor.sh`, overridable only with `SHUNT_CURSOR_AGENT_BASE_URL`. For `kind = "antigravity"`, this also governs credential-path project discovery (`loadCodeAssist`) when a stored credential has no cached project ID, not just inference; first-time onboarding (`onboardUser`) follows it too, except on the production default host, which has its own `daily-` control-plane host. When several Antigravity upstreams are configured, [`shunt login antigravity`](/reference/cli/#shunt-login-antigravity) runs that discovery against the one your routes select. For `kind = "antigravity_cli"` there is no upstream to address — the adapter runs a local `agy` binary and never reads this field — but a presetless upstream must still set it; any placeholder (the built-in provider uses `http://localhost`) will do. |
| `auth` | no | Auth mode string, or a mode-specific map. Defaults to the preset's auth, otherwise `passthrough`. |
| `effort`, `service_tier`, `classifier_model`, `count_tokens`, `websocket`, `tool_search`, `request_compression`, `retry` | no | Same per-upstream settings documented for legacy providers. Presets do not override `count_tokens`. `retry` is normalized for Cursor upstreams but does not apply to the Cursor streaming turn. |
| `workspace_roots`, `sandbox`, `profile_dir` | no | Same Antigravity controls documented for legacy providers, with the same defaults (`[]` and `true`). An ordered upstream needs them for the same reasons a `[providers.*]` entry does. |

Available presets:

| Preset | Kind | Base URL | Default auth |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | `https://api.anthropic.com` | `passthrough` |
| `codex` | `responses` | `https://chatgpt.com/backend-api` | `chatgpt_oauth` |
| `openai` | `responses` | `https://api.openai.com/v1` | `api_key`, env `OPENAI_API_KEY` |
| `xai` | `responses` | `https://api.x.ai/v1` | `api_key`, env `XAI_API_KEY` |
| `grok` | `responses` | `https://cli-chat-proxy.grok.com/v1` | `xai_oauth` |
| `kimi` | `anthropic` | `https://api.moonshot.ai/anthropic` | `api_key`, env `MOONSHOT_API_KEY` |
| `cursor` | `cursor` | `https://api2.cursor.sh` | `cursor_oauth` |
| `kimi-code` | `anthropic` | `https://api.kimi.com/coding` | `kimi_oauth` |
| `zhipu` | `anthropic` | `https://open.bigmodel.cn/api/anthropic` | `api_key`, env `ZHIPUAI_API_KEY` |
| `minimax-cn` | `anthropic` | `https://api.minimax.cn/anthropic` | `api_key`, env `MINIMAX_API_KEY` |
| `opencode` | `anthropic` | `https://opencode.ai/zen` | `api_key`, env `OPENCODE_API_KEY`, header `x_api_key` |

A bare string such as `auth = "claude_oauth"` is shorthand for `auth = { mode = "claude_oauth" }`. `api_key` maps accept `env` (required unless the preset supplies it) and `header`; an omitted `header` keeps the default (`bearer`, or the `opencode` preset's `x_api_key`). `claude_oauth`, `chatgpt_oauth`, `kimi_oauth`, and `antigravity_oauth` maps may select `account = "name"` or `accounts = [...]`, but not both. `accounts` accepts bare store-entry names and full account tables; an explicitly empty `accounts = []` is rejected, while omitting both scope fields scans the whole store. If the ChatGPT store is empty, `chatgpt_oauth` retains its `~/.codex/auth.json` fallback; if the Antigravity store is empty and nothing is configured, `antigravity_oauth` falls back to `~/.shunt/antigravity-auth.json`. `passthrough`, `xai_oauth`, and `cursor_oauth` maps take only `mode`; unknown mode-specific keys are errors.

Do not combine `[[upstreams]]` with `[providers.*]` in the config file: startup fails when both file-layer declaration forms are present. Environment variables may override individual fields by normalized upstream/provider name under either form, using `SHUNT_PROVIDERS__<name>__<field>`. Declare the ordered `[[upstreams]]` array in the config file rather than trying to synthesize the whole array with one environment variable. Legacy `[providers.<name>]` remains supported and is normalized to implicit name-sorted upstreams. Because that form has no declared failover order, it supports only zero- or one-entry model maps; use `[[upstreams]]` before adding multiple entries to a model map.

### Failover behavior

For a multi-entry model map, shunt filters the declared upstream sequence to the names in the map. It advances after an upstream status `429`, `401`, `403`, `404`, or any `5xx`, and after a failure before upstream response headers arrive. Gateway-local errors that do not represent an upstream attempt, such as auth misconfiguration or adapter-owned validation/header construction errors, return immediately so failover does not mask the configuration problem. There is no failover after `2xx` headers have been returned, including a later streaming-body failure. The Responses adapter's streaming paths commit the response before any upstream byte; a chain of `Anthropic`/`Responses` elements without the websocket transport then runs its failover inside the committed stream — pre-header transport failures and advance-status errors retry the next upstream before the synthetic start goes out — while a route that cannot advance (a terminal non-2xx, or a non-SSE success body from an Anthropic-kind winner) surfaces the failure as one terminal SSE `error` event on the committed stream. The winner's relay ends at the terminal frame (`message_stop`, or a relayed `error` frame): a streaming-body failure before it becomes one terminal `error` event, one after it surfaces nowhere, and the still-open upstream drains detached under a bounded budget, so the client's stream completes at the turn — an appended `error` event to a completed response would corrupt it. A TTFB timeout is never advanced: the configured timeout is an answer, surfacing as a terminal `504 timeout_error` event. On that committed path the response carries `content-type`, `x-gateway-model`, and — when a `[models.router]` entry routed the request or a `[models.subagents]` overlay diverted it — the router pair `x-gateway-routed-model`/`x-gateway-route-source`, decided before the first attempt and so independent of which upstream wins: the winner-dependent headers `x-gateway-upstream` and `x-gateway-upstream-model` are omitted — the winner is unknown when the headers go out with the commit — and upstream response headers (request ids, `anthropic-ratelimit-*` quota metadata included) never reach the client, even from an Anthropic-kind winner, while `x-gateway-model` stays (it names the client-requested id); request metrics record every attempt with its classified status, and stream attribution follows the winner once the stream knows it.

When the chain is exhausted, shunt returns the best relayed failure with preference `429` → `401`/`403` → `404` → other `5xx`. Pre-header failures are not remembered as best failures. If no relayed response was remembered, shunt returns a `502 api_error` with `all upstreams failed (N attempted)`.

For a `passthrough` upstream, the client's own `authorization` / `x-api-key` is forwarded on a failover attempt only when the **primary** route is itself `passthrough` and the attempt's destination origin matches that primary's. The credential is then the caller's own upstream credential, origin-specific to the primary, so a `passthrough` failover attempt on a **different** origin strips both slots and fails closed rather than replaying a host-specific token to another origin; a same-origin fallback (e.g. two passthrough entries on one host) still carries them. When the primary instead injects a credential (`api_key`/OAuth), the client headers are a gateway/client secret rather than an upstream credential, so every `passthrough` fallback strips them regardless of origin. `api_key`/OAuth upstreams inject their own server-side credential regardless of position.

Independent of origin, each retained slot is also checked by the value it actually holds: `authorization` and `x-api-key` are each cleared only when that slot's own value is shaped like a JWT shunt itself issued — three segments whose payload's `aud` claims `"shunt"`, whose `iss` claims this gateway's identity, or whose `shunt_token_use` claim is `"gateway-session"`, a dedicated marker that only shunt mints — or matches a configured `[server.auth]` client token. The JWT check is deliberately by shape, not by whether the token currently authenticates: an expired token, one minted by a sibling instance under a different `public_url`, or one that no longer verifies after a `jwt_secret` rotation is still shunt's own credential and is still cleared. The marker is an additional arm on that shape check, not a requirement: a token minted before the marker existed still matches by `aud`/`iss`, and `verify` does not require the marker either, so a token minted by an older shunt version still authenticates for as long as it remains within its TTL. An `apiKeyHelper` fills both slots with the same value, so either credential can land in either or both. A slot holding a genuine upstream credential is forwarded even when the other slot holds the gateway JWT or a static client token; only the gate-credential-bearing slot is cleared. `[server.auth] header` accepts any header name, including `authorization` itself; when it is set that way a client authenticates with a bare, unprefixed `Authorization: <token>`, so that slot is checked as a whole value as well as by its `Bearer` payload and such a token is never forwarded upstream. One caveat for that configuration: on inference requests shunt removes the configured header before routing, unconditionally, so that slot then carries nothing upstream — a caller's own credential in it is dropped too, not just a gate token. Keeping `header` at its dedicated `x-shunt-token` default avoids that collision.

Every proxied success or final failure carries `x-gateway-upstream` (selected upstream name), `x-gateway-model` (client-requested id), and `x-gateway-upstream-model` (mapped backend id) — except on the committed streaming chain path, where the response carries `content-type`, `x-gateway-model`, and the router pair below when a router routed the request or an overlay diverted it (the winner-dependent `x-gateway-upstream` and `x-gateway-upstream-model` are omitted and upstream response headers never reach the client). A response routed by a [`[models.router]`](#modelsrouter-optional) entry additionally carries `x-gateway-routed-model` (the target the router chose) and `x-gateway-route-source` (why it was chosen) — for every router type, not only the [stage router](/guides/stage-router/). A delegated turn a [`[models.subagents]`](#modelssubagents-optional) overlay diverted carries the same pair, with `x-gateway-route-source` reading `subagent_type` or `subagent`; both are omitted only when neither a router nor an overlay decided the turn. `count_tokens` uses only the first chain element, never fails over, and is left unstamped by that pair. `[server.codex_endpoint]` is pinned to its configured upstream for every model with no `[[server.codex_endpoint.routes]]` entry, and does not participate in this chain either way.

### Migrating existing configurations

Existing configurations require **no action**. Legacy providers retain their routing and name-sorted selection behavior. Three additive or deliberate behavior changes apply on upgrade:

1. Legacy providers that resolve to the same physical OAuth account now share quota windows, health, cooldown, refresh locks, and in-flight admission state. The pool persistence key schema was version-bumped, and version-2 quota caches migrate once to version 3 with independent utilization/status freshness.
2. Every proxied response gains the three `x-gateway-*` metadata headers described above.
3. On the Anthropic Messages route (`/v1/messages`), a Claude or Codex OAuth pool of any size in which every attempt fails before response headers now returns `all upstreams failed (N attempted)` instead of its pool-specific `all Claude OAuth accounts failed before receiving an upstream response` or `all Codex OAuth accounts failed before receiving an upstream response` message. The separate `[server.codex_endpoint]` inbound path is unaffected and retains the Codex-specific message.

To opt into ordered failover, rewrite each `[providers.<name>]` table as a `[[upstreams]]` entry with the same name, fold `api_key_env`, `api_key_header`, and OAuth `accounts` into the `auth` map, arrange the entries by preference, and add each participating name to the model's `upstream_model` map.

The `kimi` preset reads `MOONSHOT_API_KEY`. Older examples that explicitly used `api_key_env = "KIMI_API_KEY"` continue to work in the legacy form; an explicit upstream map also preserves that name with `auth = { mode = "api_key", env = "KIMI_API_KEY" }`. Only users relying on the preset default need to export `MOONSHOT_API_KEY`.

## `[providers.<name>]` (legacy)

Each provider is a table under a name of your choosing. Built-ins (`anthropic`, `openai`, `codex`, `xai`, `grok`, `cursor`, `gemini`, `antigravity`, `antigravity-cli`) can be partially overridden — config maps deep-merge.

| Key | Values | Meaning |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` \| `gemini` \| `antigravity` \| `antigravity_cli` | Upstream protocol / adapter. `anthropic` = Messages API (passed through, optionally re-keyed); `responses` = Anthropic Messages translated to the OpenAI Responses API (the Responses API has no `stop` parameter, so `stop_sequences` is emulated gateway-side in the translation rather than silently dropped); `cursor` = the native Cursor ConnectRPC/protobuf AgentService adapter; `gemini` = Anthropic Messages translated to Gemini `generateContent`/`streamGenerateContent` on the Google Code Assist backend; `antigravity` = the Google Antigravity backend over HTTP, which speaks the same Code Assist protocol as `gemini` but authenticates with an Antigravity subscription token and identifies itself as `ideType: ANTIGRAVITY` during project discovery; `antigravity_cli` = **deprecated** — no upstream at all, running the local Antigravity CLI binary (`agy`) as a subprocess. Because `agy` resolves its own tool calls and no `tool_use` block can ever be returned, a request that asks for one — a non-empty `tools` array, or a `tool_choice` of `any` or `tool` — is refused with a `400 invalid_request_error` rather than silently answered as text. `tool_choice: none` (even alongside `tools`), `tool_choice: auto` with no tools, and an empty `tools: []` all oblige nothing and are accepted. |
| `base_url` | URL | Upstream base; shunt appends the endpoint path. For `kind = "cursor"`, this is the login/token-refresh surface only; it does not select the agent/inference host. For `kind = "antigravity"`, credential-path project discovery (`loadCodeAssist`) also addresses this host when a stored credential has no cached project ID — not just inference — as does first-time onboarding (`onboardUser`), except on the production default host, which is onboarded through its own `daily-` control-plane host. When several Antigravity upstreams are configured, [`shunt login antigravity`](/reference/cli/#shunt-login-antigravity) runs that discovery against the one your routes select. |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `claude_oauth` \| `kimi_oauth` \| `xai_oauth` \| `cursor_oauth` \| `google_oauth` \| `antigravity_oauth` \| `none` | `passthrough` forwards the client's own credential; `api_key` injects a key from `api_key_env`; `chatgpt_oauth` reuses `~/.codex/auth.json`; `claude_oauth` selects from explicit Anthropic accounts; `kimi_oauth` selects from explicit Kimi Code accounts (`shunt login kimi`), valid only with `kind = "anthropic"` and a `kimi.com` `base_url`; `xai_oauth` reuses `~/.shunt/xai-auth.json` from `shunt login xai` (only sent to x.ai/grok.com hosts over HTTPS); `cursor_oauth` reuses `~/.shunt/cursor-auth.json` (`shunt login cursor`); `google_oauth` reuses the gemini CLI login in `~/.gemini/oauth_creds.json` and is valid only with `kind = "gemini"`; `antigravity_oauth` reuses `~/.shunt/antigravity-auth.json` (`shunt login antigravity`), is valid only with `kind = "antigravity"`, and is **not** interchangeable with `google_oauth` — Antigravity requests two scopes (`cclog`, `experimentsandconfigs`) a Gemini CLI token never carries; `none` sends no credential at all, for adapters with no upstream to authenticate against (`kind = "antigravity_cli"`). |
| `api_key_env` | env var name | Where the key is read from, when `auth = "api_key"`. Its own value can also be written as `${VAR}` / `${file:...}` (see [Secret references](#secret-references)). |
| `api_key_header` | `bearer` (default) \| `x_api_key` | Header the injected key is sent in. |
| `accounts` | array of account tables | OAuth account pool. Valid only with `auth = "claude_oauth"`, `"chatgpt_oauth"`, or `"kimi_oauth"`; see below. |
| `effort` | `low` … `max` | Optional default reasoning effort (`responses` providers). Also applies to `kind = "antigravity"`, where it is appended to a bare `gemini-*` `upstream_model` as the catalog's effort suffix. |
| `service_tier` | `fast` \| `priority` \| `flex` \| `default` | Optional default Codex "Fast" mode opt-in (`responses` providers) — sent as the Responses API `service_tier` field. `fast` normalizes to `priority`; `default` is a client-only sentinel that is never sent on the wire. Off by default. Withheld for the `xai`/`grok` flavors even when configured (xAI 400s on it). A route-level `service_tier` (including an explicit `default`) overrides this value — see below. See [Codex → Fast mode](/guides/codex/#fast-mode). |
| `count_tokens` | `tiktoken` (default) \| `estimate` | `responses` and `cursor` providers: local tiktoken count vs. `501 not_supported` fallback ([details](/guides/effort-and-context/#token-counting-count_tokens)). |
| `classifier_model` | model id | `anthropic` providers only. Upstream model for Claude Code's auto-mode permission classifier request, identified by its request shape alone — every other request keeps the model it asked for. A remap **within this provider**, never a route to another one — the key is accepted on `anthropic` upstreams only. Unset by default. See [Anthropic → Auto-mode classifier](/providers/anthropic/#auto-mode-classifier). |
| `websocket` | `true` \| `false` (default) | Opt in to the Codex Responses WebSocket v2 transport (ChatGPT/Codex backend only; falls back to HTTP on any transport failure before the first event reaches the client, so it can never do worse than plain HTTP). |
| `tool_search` | unset ("auto", default) \| `true` \| `false` | Use the native client-executed `tool_search` protocol for Claude Code's tool search on a GPT-5.4+ model, gated on flavor (non-xAI/Grok). Unset defaults to native only for known-good hosts — the ChatGPT/Codex backend and `api.openai.com` — and the text shim everywhere else, including custom OpenAI-compatible endpoints (LiteLLM, vLLM, OpenRouter, self-hosted). Set `true` to opt a verified custom endpoint into native, or `false` to always force the shim. See [Codex → Tool search](/guides/codex/#native-protocol). |
| `request_compression` | `true` (default) \| `false` | zstd-compress the Responses **request** body (`content-encoding: zstd`, level 3), matching what the Codex CLI sends to the same backend. Effective only on the ChatGPT/Codex flavor (`auth = "chatgpt_oauth"`) — no other Responses upstream is verified to accept a compressed request body, so the flag is inert there. Set `false` to send plain JSON, e.g. behind a middlebox that mishandles compressed request bodies. |
| `retry` | sub-table | Bounded retry/backoff for supported transient upstream failures. On by default (conservative); see below. Normalized but inert for the Cursor streaming turn. |
| `workspace_roots` | array of paths (default `[]`) | `kind = "antigravity_cli"` only. Roots inside which a prompt-supplied `Working directory:` may land. The system prompt is client-controlled and routinely quotes fetched documents, so it is prompt-injectable; a prompt-derived path is canonicalized (resolving symlinks and `..`) and honored **only** if it falls inside one of these roots. A path outside them is refused — the *path* is ignored, not the request, and the run falls back to the gateway's own working directory, exactly as it would had the prompt named no directory at all. Refusing the request instead would let anyone able to inject a line of system-prompt text fail every turn. Empty (the default) means no prompt-derived path is ever honored — only `SHUNT_AGY_WORKSPACE` or the gateway's own directory. |
| `sandbox` | `true` (default) \| `false` | `kind = "antigravity_cli"` only. Runs the CLI with `--sandbox`, which keeps the agent's reads and writes inside the workspace. Print mode passes `--dangerously-skip-permissions` (it cannot service an approval prompt), so without the sandbox the agent has shell access and no workspace boundary — `workspace_roots` only changes where it *starts*. Set `false` only where unrestricted terminal access is genuinely needed and the caller is trusted. **Refused at startup when combined with a non-loopback `bind`**, which would hand arbitrary local execution to anyone who can post a Messages request. The adapter also enforces this rule per request against the listener bound at boot, so a hot reload cannot disable the sandbox while a public listener remains active. Restart shunt on a loopback bind before disabling it. |
| `profile_dir` | path | `kind = "antigravity_cli"` only. Private state directory for the CLI child process. `agy` resolves its whole state tree — credentials included — through `HOME`, so pointing each provider entry at its own directory gives it its own Google account and lets several be pooled concurrently. Ambient `GOOGLE_*`/`GEMINI_*` variables are stripped from the child environment so the gateway host's own configuration cannot silently change which account — and whose billing — serves a request. Each directory needs its own one-time `agy` sign-in. The directory is created on demand and resolved to an absolute path — the child reads `HOME` after its working directory has moved to the request workspace, so a relative value would name a different directory to the CLI than the one shunt created. A leading `~` is expanded. Unset (the default) keeps the previous behavior: the CLI inherits the gateway's environment and its single ambient `~/.gemini` profile. |

### `[providers.<name>.retry]`

Bounded retry for **transient** upstream failures on supported single-credential calls: the `passthrough`/`api_key` Anthropic path and the single-credential Responses path (`api_key`, `xai_oauth`/Grok, and a `chatgpt_oauth` provider with no pooled accounts). It re-issues the request (full body, before any bytes reach the client) on connection-level transport errors (connect reset/refused, timeout). Transient response statuses are not retried on these non-idempotent creation POSTs because the upstream may already have accepted a billable generation. The current Cursor adapter's streaming turn is not wrapped in this retry layer, so its normalized `retry` table is inert and a pre-response connection failure surfaces directly. No supported path retries a `4xx` response, and retry never begins after response-body streaming starts.

Backoff is exponential with randomized (full) jitter, capped at `max_backoff_ms`. A server-supplied `Retry-After` takes precedence (both the delta-seconds and HTTP-date forms are honored); if it asks for longer than `max_backoff_ms`, the response is surfaced immediately rather than slept past budget. Retry is **held off `count_tokens`** regardless of this setting. The `claude_oauth` / `chatgpt_oauth` / `kimi_oauth` account pools drive their own account-rotation failover and are unaffected by this table.

```toml
[providers.openai.retry]
max_retries = 2          # default; 0 disables retry entirely
initial_backoff_ms = 500 # default
max_backoff_ms = 8000    # default; also caps an honored Retry-After
multiplier = 2.0         # default; exponential growth factor (>= 1.0)
```

| Key | Values | Meaning |
| :-- | :-- | :-- |
| `max_retries` | integer (default `2`, max `10`) | Extra attempts after the first. `0` disables retry. |
| `initial_backoff_ms` | milliseconds (default `500`, must be `> 0` when `max_retries > 0`) | Backoff ceiling before the first retry (jitter fills `[0, this]`), grown by `multiplier` per attempt. |
| `max_backoff_ms` | milliseconds (default `8000`, must be `> 0` when `max_retries > 0`) | Upper bound on any single backoff and on an honored `Retry-After`. |
| `multiplier` | finite number ≥ 1.0 (default `2.0`) | Exponential growth factor applied to the backoff per attempt. |

### `[[providers.<name>.accounts]]`

Explicit account entries for an Anthropic provider using `auth = "claude_oauth"`:

```toml
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
auth = "claude_oauth"

[[providers.anthropic.accounts]]
name = "primary"
credentials = "~/.claude/.credentials.json"
uuid = "00000000-0000-0000-0000-000000000000"

[[providers.anthropic.accounts]]
name = "backup"
token_env = "CLAUDE_BACKUP_OAUTH_TOKEN"
```

| Key | Required | Meaning |
| :-- | :-- | :-- |
| `name` | yes | Unique account label containing only lowercase ASCII letters, digits, and hyphens. A name-only entry resolves from the shunt-managed store. Returned to the client in `x-shunt-account` (absent on streaming Responses pool turns, whose response commits before the winning account is known); avoid personal information. |
| `credentials` | one usable source | Path to a Claude Code `.credentials.json`-shaped file. `~/` is expanded. shunt refreshes near expiry and atomically writes refreshed tokens back. |
| `token_env` | one usable source | Environment variable holding a setup token. Used verbatim and not refreshable. Mutually exclusive with `credentials`. Its own value can also be written as `${VAR}` / `${file:...}` (see [Secret references](#secret-references)). |
| `uuid` | no | Replaces an existing `metadata.user_id.account_uuid` in requests selected for this account. |
| `threshold` | no | Per-account soft quota threshold in `[0.0, 1.0]`, for every window without a per-window value. A low value marks a backup account that rotates out early. |
| `threshold_5h` / `threshold_7d` / `threshold_fable` | no | Per-window soft thresholds; each beats `threshold` for its window. See [`[server.pool]`](#serverpool-optional) for the full resolution order. |
| `priority` | no | Selection priority when the sticky account is unhealthy; lower is preferred, default `100`. Applies to Codex pools too. |
| `disabled` | no | `true` removes the account from selection entirely (kept in config and on the admin pool dashboard). Applies to Claude and Codex pools. |

A name-only entry reads `~/.shunt/accounts/claude/<name>.json`, created with `shunt login claude --name <name> --mode <mode>` (`<mode>` is one of `oauth`, `import`, or `setup-token`); the interactive CLI prompts for these three modes and recommends refreshable OAuth. `--long-lived` remains a deprecated alias for `--mode setup-token`. `SHUNT_CLAUDE_ACCOUNTS_DIR` overrides the store directory. An empty account list scans all valid store files. Refreshable OAuth/import files are updated in place when the provider rotates their refresh token, so each file must have one active owner: do not share or independently copy it across running shunt processes. Provision each process separately, or use a static setup token when appropriate. `claude_oauth` additionally requires an HTTPS `base_url` whose host is `anthropic.com` or a subdomain, preventing bearer leakage to another origin — except for loopback hosts (`localhost`, `127.0.0.1`, `[::1]`, …), which are exempt from both checks so a local debugging proxy or mock can be used over plain HTTP.

Account selection is session-sticky and quota-aware. On every upstream response handled by the `claude_oauth` account pool, shunt records `anthropic-ratelimit-unified-5h-utilization`, `anthropic-ratelimit-unified-7d-utilization`, `anthropic-ratelimit-unified-7d_oi-utilization`, their matching per-window `*-reset` headers, the aggregate `anthropic-ratelimit-unified-status`, and the matching `*-status` headers for `5h`, `7d`, and `7d_oi`. A `rejected` 5-hour or shared `7d` status makes every model near quota. A `rejected` `7d_oi` status makes Fable requests near quota. Status rejection is evaluated independently of whether the matching utilization header is present. For utilization thresholds and headroom, Fable uses `7d_oi` when that utilization is available and shared `7d` otherwise; all other families, including Sonnet, use shared `7d`. The aggregate status is a compatibility fallback only when no per-window status has been recorded. Shared 5-hour utilization at or above its threshold or the model's governing weekly utilization at or above its threshold also makes an account near quota — the threshold is the built-in `0.98` unless tuned per account (`threshold*` above) or pool-wide ([`[server.pool]`](#serverpool-optional)). shunt keeps a healthy under-threshold sticky account, but rotates off a near-quota or cooled one and prefers available under-threshold accounts by `priority`, then by soonest governing-weekly reset (or, with `[server.pool]` configured, by largest burn-rate headroom — the projected time to threshold at the observed pace minus the time to reset; `burn_rate_avoidance = true` additionally treats a negative projection as near quota), then near-quota accounts (best headroom first when `[server.pool]` is set, so an all-near pool degrades to best-margin-first while accounts past `hard_threshold` still sort last), then cooled accounts. A 429 that rejects only `7d_oi` on a Fable request creates a Fable-only cooldown; for a non-Fable request, the same `7d_oi`-only rejection creates an account-wide cooldown; `5h` or shared `7d` rejection also creates an account-wide cooldown. Utilization expires at the earlier of its own observation-time cap or that window's reset; if only its cap passes, a future reset for that window remains standalone metadata. Per-window status expires at the earlier of its own observation-time cap or the reset boundary captured when observed, and that captured boundary is cleared with the status. A stamped aggregate status has its own cap and survives unrelated window expiry, while the legacy unstamped aggregate follows window expiry. Every non-disabled account remains selectable. Reactive failover remains active. Storm control for freshly switched account concurrency is available via [`[server.pool]` `ramp_initial_concurrency`](#serverpool-optional) (off by default).

See [Anthropic Multi-Account](/guides/anthropic-multi-account/) for the complete selection and failover behavior. The behavior reference is [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude); shunt has no runtime dependency on it.

## `[[routes]]`

Legacy exact-match routing entries — checked after a matching `[models.upstream_model]` entry:

> **Legacy:** For exact model ids, prefer a `[[models]]` entry with `[models.upstream_model]`; it both routes and advertises the id as one source of truth. `[[routes]]` remains supported indefinitely, but is no longer the recommended exact-routing form.

| Key | Required | Meaning |
| :-- | :-- | :-- |
| `model` | ✅ | The exact `model` id Claude Code sends |
| `provider` | ✅ | Configured upstream name |
| `upstream_model` | — | Rewrite the model id forwarded upstream |
| `effort` | — | Per-route reasoning-effort override. On an `antigravity` route it pins the effort suffix synthesized onto a bare `gemini-*` `upstream_model`. |
| `service_tier` | — | Per-route Codex "Fast" mode override; see `[providers.*]` `service_tier` above. An explicit route-level `default` opts the route out of a provider-level `priority`/`flex` tier instead of inheriting it. |

## `[[route_prefixes]]`

Prefix-match routing entries — checked after exact routes:

| Key | Required | Meaning |
| :-- | :-- | :-- |
| `prefix` | ✅ | Model-id prefix, e.g. `gpt-` |
| `provider` | ✅ | Configured upstream name |

## `[[models]]`

Entries returned by `GET /v1/models` for [model discovery](/guides/model-discovery/). Ids must begin with `claude` or `anthropic` or Claude Code ignores them.

The top-level `auto_include_builtin_models` key defaults to `true`. When enabled, shunt returns these curated `[[models]]` entries first, then the models it discovers on its own, with exact-id duplicates removed in favor of the curated entry. Set it to `false` to expose only the `[[models]]` list — that also suppresses the upstream call described next.

Discovered models come from the live upstream list when shunt can get one. It issues `GET /v1/models` against `server.default_provider` when it is Anthropic-kind, using that provider's authentication mode. With `auth = "passthrough"`, shunt forwards the caller's credential, so each caller sees the list that credential is entitled to — except a slot holding shunt's own `[server.gateway]` JWT or a configured `[server.auth]` client token rather than a real upstream credential, which is not forwarded. `authorization` and `x-api-key` are filtered independently, so a genuine credential in the other slot is still forwarded; discovery falls back to the builtin snapshot only when neither slot has a forwardable credential left. With `api_key`, shunt uses the configured key. With `claude_oauth`, it uses the first resolvable, non-disabled account from the same effective account set as inference, including store-scanned accounts in `account_scope` order. Discovery performs no pool selection, cooldown, or quota accounting. Those two gateway-owned modes therefore expose a shared credential-scoped catalog. shunt caches nothing. When the default provider is not Anthropic-kind, there is no credential, or the call fails or times out (2 s cap), shunt falls back to a builtin snapshot of the Claude catalog. Either way these ids need no dedicated `[[routes]]` entry — they resolve through your normal routing rules, falling back to `server.default_provider` when no `[[routes]]` or `[[route_prefixes]]` entry matches.

A curated entry can include `[models.upstream_model]` to advertise, route, and translate one id in the same declaration; this is the recommended form for exact-id routing instead of `[[routes]]`. With ordered `[[upstreams]]`, the map may contain one or more `upstream = "backend-id"` pairs and resolves to a failover chain in `[[upstreams]]` declaration order. With legacy `[providers.*]`, it must contain exactly one pair because that form has no declared order. For that id the map takes precedence over `[[routes]]`, `[[route_prefixes]]`, and `server.default_provider`; each upstream's default `effort` applies to its chain element. An empty map, an empty or whitespace-only upstream name or backend id, an unknown upstream, a same-id `[[routes]]` entry, a mapped id ending in `[1m]` or `[1M]`, or a duplicate `[[models]]` id where either entry has a map is a startup error. Clients strip the context-window hint before matching, so including it in a mapped id would make that entry unreachable. Pure map-less duplicate ids retain their previous behavior, unless one of them carries a `[models.router]` table — see below.

```toml
[[models]]
id = "claude-opus-4-8"
display_name = "Claude Opus 4.8"

[models.upstream_model]
codex = "gpt-5.2"
```

| Key | Required | Meaning |
| :-- | :-- | :-- |
| `id` | ✅ | Model id exposed to Claude Code |
| `display_name` | — | Label shown in the `/model` picker |
| `upstream_model` | — | Map from configured upstream names to backend model ids; ordered `[[upstreams]]` may produce a multi-entry failover chain, while legacy providers allow one entry |

### `[models.router]` (optional)

Per-request routing for one advertised id. Instead of naming a single
destination, the entry carries a `[models.router]` table whose `type` key picks
a routing algorithm, and the algorithm picks the destination. Absent this table
and the [`[models.subagents]`](#modelssubagents-optional) overlay, a `[[models]]`
entry behaves exactly as it did before; configure neither anywhere and routing
is unchanged.

`type` — rather than shunt's usual `kind` or `mode` — is a **deliberate
exception to shunt's own naming convention**, and this is the one place the
reference says so. The routing algorithms come from
[NVIDIA-NeMo/Switchyard](https://github.com/NVIDIA-NeMo/Switchyard), and keeping
its key name means its schema documentation and its `type` values transfer here
unchanged instead of being translated twice.

Every target named by any router is an ordinary public model id, so each
resolves through the normal ladder and keeps its failover chain, account pool,
adapter, `effort`, and `service_tier`. What the client is told it got stays the
id it asked for — the chosen target travels upstream only.

| `type` | Picks by | Reads the request body |
| :-- | :-- | :-- |
| `stage_router` | Recent tool-result metadata, per turn | Yes — `tool_use.name` and `tool_result.is_error` only |
| `auto` | The same router under upstream's preset | Yes, as above |
| `random` | A weighted draw, session-sticky by default | No |
| `noop` | Nothing — answers with an empty message | No |
| `prefill_router` | A learned classifier over the latest user turn (needs the `prefill-router` build) | Yes — the text of user turns |
| `llm_classifier` | An LLM judge's verdict, per `classify_trigger`; in `mode = "escalation"`, a judge's ruling on the completed weak turn | Yes — the transcript, via the packaged or a custom prompt |
| `composite` | An LLM judge sets the tier a stage router falls open to | Yes — the transcript for the judge, tool-result metadata for the signals |
| `advisor` | One executor serves every turn; a stronger reviewer approves or sends back its terminal turns | Yes — the transcript, for the reviewer |

Two shapes serve a turn while still deciding how to route it:
`llm_classifier`'s [`mode = "escalation"`](#mode--escalation) and
[`type = "advisor"`](#type--advisor). They hold the turn until the verdict is in
and then serve it, so they are the only routes on which shunt buffers a
response the client asked to stream — see
[buffered turns](#buffered-turns-escalation-and-advisor). `prefill_router` is implemented but
**gated at compile time**: it is available only from a build that opts into the
`prefill-router` cargo feature, which is off by default — see
[below](#type--prefill_router).

`[models.router]` and `[models.upstream_model]` on the same entry are mutually
exclusive.

#### `type = "stage_router"`

Content-aware tier selection. The entry names **two** targets — a capable tier
and an efficient one — and lets the request's recent tool-result history pick
between them per turn. See the [stage router guide](/guides/stage-router/) for
how the signals and the hysteresis work.

```toml
[[models]]
id = "claude-auto"
display_name = "Auto (stage router)"

[models.router]
type = "stage_router"
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `type` | ✅ required | `stage_router` |
| `capable_target` | ✅ required | Model id for hard reasoning, investigation, and error recovery |
| `efficient_target` | ✅ required | Model id for routine production once the plan is settled |
| `picker` | `efficient_first` | Tier used when the signals are inconclusive. `efficient_first` or `capable_first` |
| `confidence_threshold` | `0.5` | Minimum scorer confidence to act on a signal, in `(0.0, 1.0]` |
| `recent_turn_window` | `3` | Assistant turns of tool results fed to the scorer. Must be at least `1` |
| `min_dwell_turns` | `3` | Turns a tier is held before a de-escalation may fire; counted from the turn that chose it, so `0` and `1` both mean no dwell floor |
| `deescalate_threshold` | `0.75` | Confidence required to move *down* a tier. The default sits above `confidence_threshold`'s, making the down direction the harder one, but the two are range-checked independently — a value below `confidence_threshold` is accepted, and warns at load |
| `session_ttl_seconds` | `3600` | How long a quiet session's pinned tier survives |
| `capable_hold_turns` | `0` | Turns the capable tier is held after a signal-driven escalation. Those turns report route source `capable_hold`, and a hold is not evidence — it cannot move a pinned tier in either direction. The default of `0` keeps pinning exactly as it was; upstream's own default is `2` |

#### `[models.router.tool_semantics]` (optional)

Four lists that extend shunt's built-in Claude Code tool vocabulary for one
router. They are applied **after** the built-in table, never instead of it, so
they reach only the names that table leaves uncategorised — `Bash`, `Skill`, and
`mcp__*` server tools. Naming a tool the built-in table already classifies as
observe, mutate, or plan (`Read`, `Edit`, `TodoWrite`, …) is a **startup
error**. So is a name carrying whitespace (`" Read "`, `"some tool"`): names are
matched exactly, so a padded one would match nothing at runtime.

```toml
[models.router.tool_semantics]
observe = ["mcp__jbcontext__code_search"]
mutate = []
plan = []
new = []
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `observe` | `[]` | Tool names that read without changing anything |
| `mutate` | `[]` | Tool names that change state; scored as a whole-file write |
| `plan` | `[]` | Tool names that plan or delegate |
| `new` | `[]` | Tool names counted as newly introduced tools by the scorer |

Editing any of the four drops that router's existing session pins on the next
load, the same way editing a threshold does.

#### `[models.router.handoff_notes]` (optional)

A system block appended to the **forwarded** request on the turns a signal moves
the tier, so the incoming model is told why it is taking over.

```toml
[models.router.handoff_notes]
escalation_note = "the previous model was stalling; pick up the diagnosis"
deescalation_note = "routine work resumes"
only_on_wrong_signal_escalation = true
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `escalation_note` | — | Appended when a signal drives the turn up to the capable tier |
| `deescalation_note` | — | Appended when the scorer hands work back down to the efficient tier |
| `only_on_wrong_signal_escalation` | `true` | Restrict `escalation_note` to the signal-driven escalations (route sources `override` and `dimensions`). Set `false` to append it on every scorer-made escalation |

The note is a new block at the **end** of the `system` array; Claude Code's
attribution block is the first element and is never touched. Sticky turns,
turns with no signal, and `count_tokens` probes carry no note — nor does a turn
that hands nothing over: the first turn of a session, and a later signal that
only re-confirms the tier already pinned. A blank note is a startup error.

**Each toggle costs a prompt-cache miss.** The system array is part of the
cached prefix, so appending or dropping the note invalidates it — on top of the
per-model prefix a tier change already forfeits. That is why the table is
opt-in, and why `only_on_wrong_signal_escalation` defaults to the narrower set.

#### `[models.router.classifier]` (optional)

An LLM **judge** for the turns the signals cannot decide. Adding this table
moves the entry onto the driven lane: on a turn the scorer leaves undecided —
one that would otherwise report route source `fall_open` — on a session no pin
is holding, shunt consults the judge and routes on its verdict. It is accepted
on `type = "stage_router"` only.

```toml
[models.router.classifier]
target = "claude-haiku-4-5"
base_threshold = 0.5
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `target` | ✅ required | Public model id of the judge. Consulted, never served — it never answers the client |
| `base_threshold` | `0.5` | Lowest `p_solve` that keeps a supported task on the efficient tier, in `(0.0, 1.0]` |
| `classify_trigger` | `every_request` | When the judge may be consulted. `every_request` allows it on any undecided turn, tool continuations included. `user_turn` allows it only when the latest message is a human user turn — `role: user` carrying at least one block that is not a `tool_result` — so a tool continuation rides the session's pin instead of paying a judge call. `new_session` behaves exactly as `every_request` here, as it does upstream: this router already holds its decision in shunt's own session pin |

The judge target is an ordinary public model id held to the same one-hop rule
as the tier targets, plus one more: it must not resolve to a **passthrough**
route. `auth = "passthrough"` means *forward the caller's credential*, and the
caller's credential is exactly what a judge call strips — so such a target
arrives with nothing and is a startup error. Every other auth mode is accepted,
including `auth = "none"`: that mode means the endpoint needs no credential at
all, so a local or self-hosted judge behind no auth is a supported
configuration, not an error. None of the caller's credential slots travel with it — the reserved
`x-shunt-*` slots and `cookie`, `authorization`, `x-api-key`, and
`anthropic-beta` are all removed. The call consumes that target's own account
pool quota, which is why a judge should map its own `[[models]]` entry.

A judged turn reports route source `llm-classifier` and pins the session like
any other decision. A judge failure of any kind — a timeout, an oversized
reply, an upstream error, an unparseable verdict, or an exhausted budget —
resolves as `fall_open`, the picker default. The judge is never consulted on a
`count_tokens` probe, and never before the request is admitted: on a turn that
consults one, inbound auth ranges over the requested id plus every target and
judge the entry can name, each with its whole failover chain, so a passthrough
answer target with a credential-injecting judge requires the client credential,
and an unauthenticated or policy-denied request makes zero judge calls. A turn
that consults no judge is gated by the chain it actually resolved — one the
signals decided on their own, and one a
[`[models.subagents]`](#modelssubagents-optional) overlay diverted before the
router ran.

#### Per-call bounds

Six keys bound every internal call an entry makes. They live on the table that
makes those calls: `[models.router]` of any driven type — a `stage_router`
carrying a `classifier`, `llm_classifier`, `composite`, or `advisor` — and a
classifier-form [`[models.subagents]`](#modelssubagents-optional) overlay, which
carries its own copy. Crossing one cancels the upstream call. Each must be at
least `1`; a `0` is a startup error naming the key.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `judge_timeout_ms` | `30000` | End-to-end deadline on the non-streaming judge call — headers *and* body, so a `200` that then stalls is cut here rather than left hanging |
| `judge_max_response_bytes` | `65536` | Largest judge reply collected; a larger one resolves as `fall_open` |
| `gated_max_bytes` | `8388608` | Largest retained turn: its SSE frame bytes, or its JSON body |
| `gated_idle_ms` | `60000` | Longest gap between completed content frames of a retained turn. SSE keep-alives (`event: ping` frames and `:` comment frames) do not reset it, and a frame split across chunks is reassembled before it is classified |
| `gated_max_duration_ms` | `600000` | Wall-clock ceiling on a retained turn, headers and body |
| `max_judge_calls` | `8` | Judge calls one session may make. The gated turn itself is not a judge call and is not counted |

The three `gated_*` keys bound the **gated** turns of an
[`escalation`](#mode--escalation) or [`advisor`](#type--advisor) entry — the
turns shunt holds until a verdict is in. On every other entry there is no gated
turn, and they bound nothing.

#### `type = "llm_classifier"`

An LLM **judge** decides the whole turn, rather than stepping in only where
signals ran out. The entry names the judge, the destinations it may pick, and
`mode` — which of three verdict shapes the judge produces. `capability` and
`custom` are described here; `escalation` judges a completed turn instead and
has [its own section](#mode--escalation).

`mode` is **required**, which is a deliberate departure from the upstream
schema, where it defaults to `capability`: the three modes route on different
principles, and one of them (`escalation`) buffers the turn it serves, so an
omitted `mode` must not silently pick one.

**`mode = "capability"`** — the packaged judge returns a solve probability for
the task. A probability at or above `base_threshold` keeps the turn on
`weak_target`; below it, the turn goes to `strong_target`.

```toml
[[models]]
id = "claude-judged"

[models.router]
type = "llm_classifier"
mode = "capability"
classifier_target = "claude-haiku-4-5"
strong_target = "claude-opus-4-8"
weak_target = "claude-sonnet-4-6"
base_threshold = 0.5
# threshold_step = 0.0
# classify_trigger = "every_request"
# message_hash_fallback = false
# recent_turn_window = 3
# max_output_tokens = 4096
# prompt = "…"
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `type` | ✅ required | `llm_classifier` |
| `mode` | ✅ required | `capability` |
| `classifier_target` | ✅ required | Public model id of the judge. Consulted, never served |
| `strong_target` | ✅ required | Model id for a task the judge is not confident about |
| `weak_target` | ✅ required | Model id for a task the judge expects to be solved |
| `base_threshold` | ✅ required | Lowest solve probability that still routes to `weak_target`, in `(0.0, 1.0]` |
| `threshold_step` | `0.0` | Finite and non-negative; added once for an uncertain or unmatched verdict and twice for an unsupported one. `base_threshold + 2 × threshold_step` must be at most `1.0` |
| `prompt` | packaged prompt | Replaces the packaged capability prompt. The schema is sent separately as structured-output configuration, so the prompt must not contain `{{RESPONSE_SCHEMA}}`, and must not be blank |

**`mode = "custom"`** — you supply the prompt and the JSON Schema, and a JSON
Pointer picks a **model group name** out of the verdict. The first model of that
group serves the turn. `any` and `judge` are reserved and required; every other
group name is yours, which is how one entry chooses between more than two
models.

```toml
[models.router]
type = "llm_classifier"
mode = "custom"
models = { judge = ["claude-haiku-4-5"], capable = ["claude-opus-4-8"], efficient = ["claude-sonnet-4-6"], any = ["claude-sonnet-4-6", "claude-opus-4-8"] }
default_target = "efficient"
prompt = "Select exactly one target for this turn. Return only JSON matching the response schema."
response_schema = '''
{"type": "object",
 "properties": {"target": {"type": "string", "enum": ["capable", "efficient"]}},
 "required": ["target"],
 "additionalProperties": false}
'''
policy = { type = "target_selector", selector = "/target" }
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `type` | ✅ required | `llm_classifier` |
| `mode` | ✅ required | `custom` |
| `models.any` | ✅ required | Every selectable destination. Every other answer group's targets must also appear here (`judge` is exempt); one that does not is a startup error |
| `models.judge` | ✅ required | One or more ordered judge candidates. Consulted, never served |
| `models.<name>` | — | A group you name; a verdict naming it selects the group's first model |
| `default_target` | ✅ required | Group used when the judge returns no usable verdict. Any configured group except `judge`, and it must be non-empty |
| `prompt` | ✅ required | Judge system prompt. Must be non-blank and must not contain `{{RESPONSE_SCHEMA}}` — the schema is sent separately |
| `response_schema` | ✅ required | The inner JSON Schema as a TOML string. Must parse as a JSON object; shunt adds the provider wrapper |
| `policy` | ✅ required | `{ type = "target_selector", selector = "…" }`, where `selector` is a JSON Pointer into the verdict, such as `/target` |

Both modes share these, and the six [per-call bounds](#per-call-bounds):

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `classify_trigger` | `every_request` | When the judge runs. `every_request` judges every turn, tool continuations included. `user_turn` judges each new human user turn and retains that target across the tool calls between them. `new_session` judges once and reuses that target for the session |
| `message_hash_fallback` | `false` | Keys retention on the first user message, for clients that send no session id. Requires `classify_trigger = "new_session"`; setting it on another trigger is a startup error |
| `recent_turn_window` | unset | When set, trailing turns the judge additionally sees. Must be at least `1` |
| `max_output_tokens` | `4096` | Completion-token ceiling on the judge verdict. Must be at least `1` |

**When the judge does not answer.** A judge call that fails in any way — a
timeout, an oversized reply, an upstream error, a `400`, an unparseable
verdict — produces no verdict, and the turn goes to the algorithm's own
default: `strong_target` in `capability` mode, `default_target`'s first model in
`custom` mode. Exactly **one** judge call is made per consulted turn, against
the first judge candidate, so a failure is not retried down `models.judge`: the
turn is answered, the route source is `classifier_fail_open`, and the client
still gets its `200`.

**Sessions live in the algorithm.** `classify_trigger` retention is upstream's
state and is held inside the router instance, which shunt builds once per
loaded configuration. A hot reload rebuilds it, so a reload forgets which target
each session was holding — the same property `prefill_router` has.
`max_judge_calls` is shunt's own and is counted per `(session, agent)`, so a
delegated child spends its own budget rather than its parent's; a request
carrying no session id is not tracked, so the bound applies per request for it.
A turn whose budget is spent skips the judge and takes the fail-open target,
recorded as judge-call outcome `budget_exhausted`.

**Probes resolve without a judge.** A `count_tokens` request never consults one
and is answered from the fail-open target, as are the surfaces with no request
body — `GET /routes`, `/v1/models` discovery, and `shunt check` — which report
it under route source `classifier_default`.

Every target and every judge is an ordinary public model id under the same
one-hop rule as the stage router's, and a judge must not resolve to a
**passthrough** route for the reason given
[above](#modelsrouterclassifier-optional): a judge call carries none of the
caller's credentials, so a passthrough route has nothing to run on.

#### `mode = "escalation"`

The third `llm_classifier` mode starts each session on a weak target and has a
judge read how the work is going. Each turn on a session that has not latched
is made on `weak_target` and held. The judge then rules on the **completed**
turn — the work the weak model actually did, not a prediction. A decline resets
the escalate streak. An escalate verdict extends it. While the streak is below
`confirmations`, the held weak turn is served. When it reaches `confirmations`,
the session latches: that turn's weak answer is discarded and `strong_target`
serves it, and every later turn of the session goes straight to
`strong_target` with no judge call and no buffering.

```toml
[[models]]
id = "claude-escalate"

[models.router]
type = "llm_classifier"
mode = "escalation"
classifier_target = "claude-haiku-4-5"
strong_target = "claude-opus-4-8"
weak_target = "claude-sonnet-4-6"
# prompt = "…"
# max_output_tokens = 4096

[models.router.escalation]
confirmations = 2
# recent_turn_window = 28
# window_message_chars = 500
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `type` | ✅ required | `llm_classifier` |
| `mode` | ✅ required | `escalation` |
| `classifier_target` | ✅ required | Public model id of the trajectory judge. Consulted, never served |
| `strong_target` | ✅ required | Model id served once the session latches |
| `weak_target` | ✅ required | Model id served before the latch. It may be the same id as `classifier_target` |
| `prompt` | packaged prompt | Replaces the packaged trajectory-judge prompt |
| `max_output_tokens` | `4096` | Completion-token ceiling on the judge verdict. Must be at least `1` |
| `escalation.confirmations` | `2` | Consecutive escalate verdicts required to latch. Must be at least `1`. Above `1` needs a session id: without one every turn starts from zero and the session never latches |
| `escalation.recent_turn_window` | `28` | Trailing messages shown to the judge. Must be at least `1` |
| `escalation.window_message_chars` | `500` | Per-message character cap inside that window. Must be at least `50` |

The `[models.router.escalation]` table is optional; leaving it out keeps the
three defaults, which are upstream's benchmarked configuration. The six
[per-call bounds](#per-call-bounds) go on `[models.router]`. A classifier-form
[`[models.subagents]`](#modelssubagents-optional) overlay stays
`mode = "custom"` only.

`weak_target` may be a **passthrough** route, although `classifier_target` may
not: the weak turn is the client's own answer, so it carries the caller's
credential exactly as a live turn does. A `count_tokens` probe makes no judge
call and no gated call, and answers from `weak_target`. Judge calls are counted
by `shunt.router.judge_calls{algorithm="llm_classifier"}`.

See [buffered turns](#buffered-turns-escalation-and-advisor) for how the held
turn is served, what the client sees on each outcome, and what it costs.

#### `type = "composite"`

A judge sets the tier a stage router falls open to, and leaves the signal
scoring alone. The stage table takes **no `picker`** — the classifier supplies
that tier — so a `picker` key here is a startup error.

```toml
[[models]]
id = "claude-composite"

[models.router]
type = "composite"

[models.router.classifier]
target = "claude-haiku-4-5"
base_threshold = 0.5
classify_trigger = "user_turn"

[models.router.stage]
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
confidence_threshold = 0.5
# recent_turn_window = 3
# capable_hold_turns = 0
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `type` | ✅ required | `composite` |
| `classifier.target` | ✅ required | Public model id of the tier judge. Consulted, never served |
| `classifier.base_threshold` | ✅ required | Lowest `p_solve` that still routes to the efficient tier, in `(0.0, 1.0]` |
| `classifier.classify_trigger` | ✅ required | `user_turn` re-picks the tier on each human user turn; `new_session` picks once and holds it. `every_request` is **rejected** here — a judge call per tool step is the cost this type exists to avoid |
| `classifier.message_hash_fallback` | `false` | Retains the tier by hashing the first user message, for clients that send no session id |
| `stage.capable_target` | ✅ required | Capable tier |
| `stage.efficient_target` | ✅ required | Efficient tier |
| `stage.confidence_threshold` | ✅ required | Corroboration a decisive signal needs, in `(0.0, 1.0]` |
| `stage.recent_turn_window` | `3` | Trailing tool results the signals are computed over. Must be at least `1` |
| `stage.capable_hold_turns` | `0` | Turns the capable tier is held after a signal-driven escalation; shunt's default is `0`, upstream's is `2` |
| `stage.tool_semantics` | — | The same four lists as [`[models.router.tool_semantics]`](#modelsroutertool_semantics-optional), under the same rules |

The six [per-call bounds](#per-call-bounds) go on `[models.router]`, not inside
either sub-table. A turn the classifier cannot reach falls open to
`stage.efficient_target`, which is upstream's rule and is also what a probe and
a body-less surface report.

Because the stage half is libsy's own stage route, its decisive turns keep the
stage router's route sources rather than reporting as classifier decisions —
so a composite's signal-driven turns read the same way a plain `stage_router`'s
do.

#### `type = "advisor"`

One **executor** serves every client-visible turn. A stronger **advisor**
reviews the executor's terminal turns — a plan before the work, or a claim that
the task is done — before the client sees them. APPROVE releases the held turn.
REDO discards it and sends the executor back to work with the advisor's plan.
The advisor never serves a turn, so the client only ever sees executor output.

```toml
[[models]]
id = "claude-reviewed"

[models.router]
type = "advisor"
executor_target = "claude-sonnet-4-6"
advisor_target = "claude-opus-4-8"
gate_trigger = "no_tool_call"
max_reviews = 1
# gate_stall_turns = 0
# gate_min_tool_results = 0
# advisor_max_tokens = 2048
# transcript_max_chars = 200000
# fail_open = true
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `type` | ✅ required | `advisor` |
| `executor_target` | ✅ required | Serves every client-visible turn |
| `advisor_target` | ✅ required | Reviews gated turns. Never served |
| `gate_trigger` | `no_tool_call` | What fires a review: `no_tool_call`, the executor's first turn that ends without a tool call, or `pattern` |
| `gate_trigger_pattern` | unset | Regex for the `pattern` trigger, searched rather than anchored. Required and non-empty under `pattern`; setting it under `no_tool_call` is a startup error |
| `max_reviews` | `1` | Reviews allowed per session. Must be at least `1`. A request without `x-claude-code-session-id` counts as a session of its own, so sessionless callers never share one budget |
| `gate_stall_turns` | `0` | Reviews one turn as a mid-task checkpoint once the conversation carries this many assistant turns. `0` turns it off |
| `gate_min_tool_results` | `0` | Tool results a conversation needs before a `no_tool_call` turn is reviewable |
| `advisor_max_tokens` | `2048` | Output-token ceiling on each review. Must be at least `1` |
| `advisor_temperature` | unset | Sampling temperature for reviews. Omitted from the review request when unset |
| `transcript_max_chars` | `200000` | Cap on the transcript sent to the advisor; a longer one is trimmed from the middle. Must be at least `256` |
| `fail_open` | `true` | When a review fails, serve the held turn. `false` fails the request with a `502` instead |
| `reviewer_system_prompt` | packaged prompt | Replaces the APPROVE/REDO reviewer prompt |
| `redo_feedback_prefix` | packaged prompt | Replaces the text placed in front of a REDO plan fed back to the executor |

The six [per-call bounds](#per-call-bounds) go on `[models.router]`.

**Which turns are held.** Whether a turn trips `gate_trigger` is known only once
the turn is complete, so while the session still has review budget **every**
executor turn is held, and the ones that do not trip the gate are served
without a review. Once `max_reviews` is spent, or the advisor has failed three
consults in the session (a failed consult refunds its review), the executor
streams live with no buffering for the rest of the session.

**REDO.** The held turn is discarded before any response header reaches the
client. The discarded turn and the advisor's plan are appended to the
conversation, and the executor is re-run. That re-run streams live.

`executor_target` may be a **passthrough** route, although `advisor_target` may
not, for the same reason as escalation's weak target. A `count_tokens` probe
makes no review and no gated call, and answers from `executor_target`. Reviews
are counted by `shunt.router.judge_calls{algorithm="advisor"}`, and `GET
/routes` lists `advisor_target` under `judges`.

#### Buffered turns: escalation and advisor

A **gated** turn — escalation's weak turn before the latch, or an advisor
executor turn while the session has review budget — is made first, held, and
served only once the verdict is in. Every other turn on these entries, and every
turn on every other route, streams exactly as before.

**The caller's mode is kept.** A `stream: true` caller's gated call streams.
Its SSE frames are retained as they arrive and, if the turn is served, replayed
byte for byte. A `stream: false` caller's gated call is non-streaming, and the
caller gets the single JSON message. The gate changes *when* the answer is
sent, never its shape. The replayed `message_start.model` is the router's own
id, not the executor's, on an Anthropic and an OpenAI Responses executor alike,
so Claude Code's `/model` display and `--resume` see the id they asked for.
Response headers are committed only when the replay starts.

**Only a complete turn is served.** A held turn is servable only after its
terminal marker: `message_stop` on a streaming call, or a complete body that
parses as one message on a non-streaming call. A truncated `200` is never
replayed. A turn ends at its `message_stop` frame, as the live stream does:
nothing after it is replayed, and a connection that breaks or stays open after
it does not cut the turn. Gated turns take the target's ordered failover chain.

`x-gateway-route-source` — and the `source` label on
`shunt.router.decisions` — says what happened:

| Source | Entry | Meaning | Delivery |
| :-- | :-- | :-- | :-- |
| `escalation_weak` | escalation | The judge let the weak turn through: it declined, or the escalate streak is still below `confirmations` | Replayed |
| `escalation_latch` | escalation | The session latched, on this turn or earlier, so the strong target served the turn | Live |
| `escalation_fallback` | escalation | The weak turn failed or was cut before its terminal marker, so the strong target served the turn. The judge was not called | Live |
| `classifier_fail_open` | escalation | The judge failed after a complete weak turn, so the weak turn was served | Replayed |
| `advisor_approve` | advisor | The executor turn was reviewed and approved | Replayed |
| `advisor_pass` | advisor | The executor turn was served without a review: it did not trip the gate — it ends in a tool call, for example — or no review could be reserved | Replayed |
| `advisor_fail_open` | advisor | The review failed after a complete executor turn, so the turn was served | Replayed |
| `advisor_redo` | advisor | The reviewer said REDO. The discarded turn was never sent; this is the executor's re-run | Live |
| `advisor_exhausted` | advisor | The session's `max_reviews` is spent, or its advisor has failed three consults (a failed consult refunds `max_reviews`), so the executor streams with no buffering | Live |
| `gated_error` | either | The gated turn could not be served — see below | Error |

**When something fails:**

| What fails | `escalation` | `advisor` |
| :-- | :-- | :-- |
| The gated turn crosses a `gated_*` bound, or ends before its terminal marker | Discarded before any header is sent; the strong target serves the turn live (`escalation_fallback`) | Discarded before any header is sent; the request fails with a gateway-owned `502` in the Anthropic error shape (`gated_error`). It is not a REDO and not a failover attempt: the upstream already answered `2xx` |
| The gated call's upstream answers with an error status | Relayed to the client unchanged, as a live turn's would be (`gated_error`) | Relayed unchanged (`gated_error`) |
| The judge or review fails after a complete turn — a timeout, an oversized or unparseable reply, an upstream error, or `max_judge_calls` spent | The weak turn is served (`classifier_fail_open`) | With `fail_open = true`, the executor turn is served (`advisor_fail_open`); with `fail_open = false`, the request fails with a gateway-owned `502` (`gated_error`) |

**What it costs.** You opt into these per entry:

- On a gated turn the client receives nothing until the whole turn is complete
  and judged, so time to first token becomes time to last token.
- Escalation makes a judge call on every turn before the latch. A turn that
  latches also pays for the weak call it discards.
- A discarded weak or executor turn still consumed its upstream's quota. It is
  the client's own answer dispatch, so it counts as `caller="client"` in
  `shunt.requests`.

#### `type = "auto"`

Upstream's stage-router preset: `picker = "efficient_first"` and
`confidence_threshold = 0.5`, with every other stage key at its shunt default.
Only the two targets are accepted beside `type`; set any other stage key and use
`type = "stage_router"` instead. That includes `[models.router.classifier]` and
the per-call bounds — the preset carries no judge, and a classifier table on an
`auto` entry is a startup error.

```toml
[[models]]
id = "claude-quick"

[models.router]
type = "auto"
capable_target = "claude-opus-4-8"
efficient_target = "claude-sonnet-4-6"
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `type` | ✅ required | `auto` |
| `capable_target` | ✅ required | As for `stage_router` |
| `efficient_target` | ✅ required | As for `stage_router` |

#### `type = "random"`

A weighted split across two or more targets — a canary aid. By default one
Claude Code session stays on one arm.

```toml
[[models]]
id = "claude-canary"

[models.router]
type = "random"
targets = ["claude-sonnet-4-6", "gpt-5.6-terra"]
weights = [9, 1]
# seed = 0
# affinity = "session"
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `type` | ✅ required | `random` |
| `targets` | ✅ required | Model ids to split across |
| `weights` | equal | One positive-or-zero weight per target; a weight of `0` disables that target. Each weight must be finite, and so must their sum — a list that overflows to infinity is rejected at load |
| `seed` | `0` | Hash salt under `session` affinity; the draw seed under `request` affinity |
| `affinity` | `session` | `session` keeps one session on one arm; `request` draws per request |

Under `affinity = "session"` the arm is `sha256(seed ‖ model ‖ session id)`
scaled into the weight range. Nothing is stored, so the arm survives restarts
and is identical across replicas loading the same config — and changing `seed`,
`targets`, or `weights` can move it. A request that sends no
`x-claude-code-session-id` takes a fresh weighted draw instead of sharing one
arm, so a 90/10 split stays 90/10 for clients that send no session. Under
`affinity = "request"` every request draws, and a set `seed` makes the sequence
reproducible.

Session affinity is **stickiness, not access control.** The session id comes
from the client, so a caller that retries ids can steer itself onto the arm it
wants — which guards nothing, because every target is a public model id that
same caller may simply ask for by name. Use the managed-model policy for access.

Surfaces with no request body — `GET /routes`, `/v1/models` discovery, and
`shunt check` — report the first target with a positive weight.

#### `type = "noop"`

Answers without calling any upstream: an empty terminal assistant message in the
caller's own mode. For `stream: true` that is a valid SSE sequence
(`message_start`, `message_delta` with `stop_reason: "end_turn"`,
`message_stop`); otherwise one Message JSON object. `count_tokens` answers
`input_tokens: 0`. The route is authenticated exactly like any other, so this is
not an unauthenticated hole — it is a smoke test for client wiring that proves
the inbound path end to end without spending a token.

```toml
[[models]]
id = "claude-noop"

[models.router]
type = "noop"
```

`type` is the only key it accepts.

#### `type = "prefill_router"`

A learned router. Upstream's own classifier scores the latest text user turn
and picks one of the entry's targets, running its model in this process rather
than calling a judge upstream.

**This one needs a build that has it.** `prefill_router` is compiled in only
behind the `prefill-router` cargo feature, which is **off by default** and
which the release workflow never enables — so no release binary and no
Homebrew install has it. Build from source:

```sh
cargo build --release --features prefill-router          # add ,ui for the dashboard
```

The config below parses in every build. What differs is the load: on a binary
without the feature the load fails, so `shunt check` reports it rather than the
gateway starting without the algorithm it was configured for. That feature
error comes ahead of every other complaint about the table, so the key-level
rules (a blank target, an empty `checkpoint`, a non-positive `max_length` or
`batch_size`) are what a build with the feature on reports.

```text
models entry <id> router type = "prefill_router" is not compiled into this binary: it needs the `prefill-router` cargo feature, which is off by default and absent from release binaries; build from source with `cargo build --features prefill-router` (docs/routing-algorithms.md)
```

```toml
[[models]]
id = "claude-learned"

[models.router]
type = "prefill_router"
targets = ["claude-sonnet-4-6", "claude-opus-4-8"]
checkpoint = "/models/router.pt"
# device = "cpu"
# cache_dir = "/var/cache/huggingface"
# max_length = 2048
# batch_size = 32
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `type` | ✅ required | `prefill_router` |
| `targets` | ✅ required | Model ids to choose between, in the checkpoint's head order |
| `checkpoint` | ✅ required | Path to the tensor-only router checkpoint; a relative path resolves against the process working directory |
| `device` | auto-detected | Torch device the router runs on — `cpu`, `cuda`, `cuda:0` |
| `cache_dir` | — | Hugging Face cache directory for the encoder and its tokenizer |
| `max_length` | `2048` | Maximum tokenized encoder input length; longer input is truncated. Must be greater than `0`. Unset leaves upstream's own default |
| `batch_size` | `32` | Maximum prompts per encoder forward pass. Must be greater than `0`. Unset leaves upstream's own default |

**What the operator has to supply.** The feature embeds Python through PyO3,
so the build links libpython and the running gateway needs `torch`,
`transformers`, `numpy`, and `accelerate` importable in the interpreter it
embedded — set `PYO3_PYTHON` at build time to that interpreter (3.10 or newer,
with a shared libpython) rather than letting PyO3 take the first `python3` on
`PATH`. It also needs a router checkpoint: Switchyard v0.3.0 ships no
checkpoint, exporter, or encoder assets, so obtaining or training a compatible
one is the operator's job. When either is missing the gateway refuses to
start, and a hot reload that hits the same problem is refused with the running
config left in place:

```text
models entry <id> router type = "prefill_router" failed to load: <upstream error>
```

A reload rebuilds the router, and the per-session affinity lives inside it, so
a reload forgets which target each session was on.

**Admission comes first.** The router is driven only after `[server.auth]` and
the gateway's managed-model policy have admitted the request. Inbound
authentication ranges over every target the entry names rather than the one it
will pick, so a caller must authenticate if any target injects a credential;
the managed-model policy checks the requested id alone, so `availableModels`
lists this id and never its private targets. A caller either gate refuses
therefore triggers no inference and seeds no affinity for the session id it
sent.

Read the first half of that as the condition it is. An entry whose targets are
*all* passthrough injects no credential anywhere in its envelope, so inbound
authentication has nothing to demand and admits every caller — including an
anonymous one, which then drives the router. If the drive itself is what you
mean to protect, give the entry a credential-injecting target; neither
`[server.auth]` nor gateway login gates an all-passthrough entry.

**How a turn is decided.** Only `user` and `assistant` roles and only `text`
and `tool_result` blocks are handed to the algorithm; it scores the latest text
user turn, and treats a message whose blocks are all `tool_result` as a tool
continuation rather than a new human turn. Session identity comes from
`x-claude-code-session-id`, plus `x-claude-code-agent-id` for a delegated
child, so a continuation reuses the turn's decision instead of re-running
inference; a caller that sends neither falls back to upstream's hash of the
first user message. Inference runs on a blocking worker, one prediction at a
time per entry. `count_tokens` probes are decided the same way, and on a
continuation that is an affinity hit rather than an inference.

`x-gateway-route-source` — and the `source` label on
`shunt.router.decisions{algorithm="prefill_router"}` — reports which of the
three happened:

| Source | Meaning |
| :-- | :-- |
| `prefill` | The router decided the turn, by inference or by session affinity |
| `prefill_fail_open` | The routing call errored, so the request went to the default target: upstream's rule is the first entry in `targets` |
| `prefill_default` | A surface with no request body — `/v1/models` discovery, `GET /routes`, model resolution — has no turn to score, so it reports the first target |

`GET /routes` lists the entry with `algorithm: "prefill_router"` and its
targets. The `shunt.stage_router.*` metrics stay the signal-only router's and
never gain prefill rows.

#### Validation

A target that is itself a router, a blank target, a threshold outside
`(0.0, 1.0]`, a `recent_turn_window` of `0`, a router **id** ending in `[1m]` or
`[1M]`, a duplicate `[[models]]` id where either entry carries a router table, a
`type` this build does not implement, or the same entry also declaring
`[models.upstream_model]` is a startup error. On a `prefill_router` entry an
empty `targets`, the same target listed twice, a blank `checkpoint`, and a
`max_length` or `batch_size` of `0` are startup errors too — those are what a
build with the feature **on** reports, because a build without it refuses the
entry by naming the missing cargo feature before any of them is reached; and
repeated targets are compared after the trailing `[1m]`/`[1M]` hint is stripped
like every other target comparison. Two map-less entries may otherwise
share an id, but a router names a routing policy rather than discovery metadata,
so a duplicate would leave two policies for one id. Target ids are compared
after the trailing `[1m]`/`[1M]` hint is stripped, the same way routing matches
them — so a target resolving to any entry that carries its own router is
rejected whatever the two types are, which is what keeps resolution one hop. A
[`[models.router.classifier]`](#modelsrouterclassifier-optional) target is held
to that same one-hop rule and to two more checks:
`classifier.base_threshold` is range-checked exactly like
`confidence_threshold`, and the judge must not resolve to a passthrough route
— a target whose effective chain contains a passthrough upstream is a startup
error, because the caller's credential is stripped from a judge call and a
passthrough route has nothing else to run on. `auth = "none"` is accepted: an
endpoint that needs no credential is not the same as one whose credential went
missing. Any of the
six [per-call bounds](#per-call-bounds) set to `0` is a startup error naming
the key.

Four shapes warn instead of failing the load, each because it has a coherent
operator intent: a target that matches no explicit route (it still resolves
through `server.default_provider` like any other unmatched id, and this warning
now covers every router type), `capable_target` and `efficient_target` resolving
to the same id (both tiers deliberately flattened onto one model), a
`deescalate_threshold` below `confidence_threshold` (de-escalation made the
easier direction, which a cost-first deployment may want), and a `[[routes]]`
entry naming the router's own id (inert, since the router decides that id's
destination). A `[[route_prefixes]]` entry the id merely starts with is **not**
reported — it still serves every other id matching it. Each is emitted once per
load — and a hot reload is a load, so a config left unfixed warns again on each
one.

### `[models.subagents]` (optional)

An overlay for **delegated work** on any `[[models]]` entry: one with a
`[models.upstream_model]` map, one with a `[models.router]` table, or a map-less
id that resolves through `[[routes]]`. A `Task` sub-agent, a hook agent, or a
workflow sub-agent requesting the id is diverted to the overlay's target. The
parent session's own turns never see the table and resolve the entry exactly as
they did without it. The table sits on the entry rather than inside `router`
because a fixed entry has no router table, and Switchyard's "passthrough with
subagents" is exactly a fixed entry here.

```toml
[[models]]
id = "claude-opus-4-8"

[models.upstream_model]
anthropic = "claude-opus-4-8"

[models.subagents]
type = "passthrough"
target = "claude-haiku-4-5"
by_type = { Explore = "claude-haiku-4-5", fork = "claude-sonnet-4-6", teammate = "claude-sonnet-4-6" }
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `type` | ✅ required | `passthrough`, the fixed form above, or [`llm_classifier`](#subagents-type--llm_classifier), where a judge picks the child's target |
| `target` | ✅ required (`passthrough`) | Model id a delegated turn goes to when `by_type` names nothing for its agent type — and where every delegated turn goes when the agent-type header is not sent |
| `by_type` | `{}` | Agent type → model id, keyed on the literal `x-claude-code-agent-type` value |

**What counts as delegated work.** A request whose `x-claude-code-request-class`
is `subagent` or `workflow`; when that header is absent, a request carrying a
non-blank `x-claude-code-agent-id`, which Claude Code sends on every delegated
turn regardless of the hint gate. The class is authoritative when sent: `main`
with an agent id is main traffic, and `compaction` and `auxiliary` are harness
maintenance — none of the three ever takes the overlay. So on a default
deployment, where the class and type headers are gated off, every `Task` child
takes `target`; `by_type` needs the client to set
`CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`.

**`by_type` keys** are matched exactly, case included. A built-in agent's id
travels verbatim — `Explore`, `Plan`, `general-purpose`, `claude`, and `fork`
(which the client only offers under `CLAUDE_CODE_FORK_SUBAGENT=1`). A project
agent from `.claude/agents/` arrives as `custom`; its own name is never sent, so
`custom` is the only key it can match. `teammate` is the client's literal for an
Agent Teams member and has not been observed on the wire. A blank key, or one
carrying whitespace, is a startup error: it could never match.

**Targets** are ordinary public model ids under the same one-hop rule as router
targets: `target` and every `by_type` value must not resolve, after the trailing
`[1m]`/`[1M]` hint is stripped, to an entry carrying its own `[models.router]` or
`[models.subagents]` table — and a router target may not resolve to an entry
carrying this overlay. A blank target, an overlaid id ending in `[1m]` or `[1M]`,
and a duplicate `[[models]]` id where either entry carries the table are startup
errors. A target matching no explicit route warns at load, as a router target
does, and still resolves through `server.default_provider`.

**No state.** The target is a function of the config and the request's headers
alone: no session pin, no store, no judge call. On a router-backed id the child
is diverted before the router runs, so a child's turns are never scored against
the transcript and never touch the parent's pin. A diverted turn carries
`x-gateway-routed-model` (the target) and `x-gateway-route-source` —
`subagent_type` for a `by_type` hit, `subagent` for the `target` fallback — and
is counted in `shunt.router.decisions` with `algorithm = "subagents"`. Surfaces
with no request resolve nothing: `/v1/models` discovery and `shunt check` carry
no per-model destination information at all, and `GET /routes` shows the
parent's own `[[routes]]`/`[models.router]` entry only when one exists — an id
left to `server.default_provider` appears in neither array. None of the three
resolves the overlay's diverted target, and the overlay itself is never listed
in the `routers` array.

#### subagents `type = "llm_classifier"`

The overlay's second form: instead of a fixed target, a judge reads the
delegated task and names the group that serves it. Only `mode = "custom"`
exists here — `mode = "capability"` is a startup error — and the keys are the
`custom` mode's, described in full [above](#type--llm_classifier).

```toml
[models.subagents]
type = "llm_classifier"
mode = "custom"
models = { judge = ["claude-haiku-4-5"], capable = ["claude-opus-4-8"], efficient = ["claude-sonnet-4-6"], any = ["claude-sonnet-4-6", "claude-opus-4-8"] }
default_target = "efficient"
classify_trigger = "new_session"
max_output_tokens = 64
prompt = """
Select exactly one target for the delegated task.

- Select "capable" for code review, critique, auditing, or correctness analysis.
- Select "efficient" for implementation, research, explanation, and other delegated work.

Return only JSON matching the response schema.
"""
response_schema = '''
{"type": "object",
 "properties": {"target": {"type": "string", "enum": ["capable", "efficient"]}},
 "required": ["target"],
 "additionalProperties": false}
'''
policy = { type = "target_selector", selector = "/target" }
```

Three rules differ from the `[models.router]` form:

- **`classify_trigger` defaults to `new_session`,** and `user_turn` is
  rejected. A delegated child is one task, so its target is picked once and held
  for the rest of it; re-judging per user turn would spend a judge call on a
  decision that cannot change.
- **`message_hash_fallback` must be `false`.** The classification is already
  keyed on `(session, agent)`, so hashing the first message instead would key
  two different children of one session onto one verdict.
- **The parent is never classified.** What counts as delegated work is exactly
  what it is for the `passthrough` form above, so a parent turn, a `main` turn
  carrying an agent id, and the `compaction` and `auxiliary` classes all resolve
  the entry as if the table were absent — and make no judge call.

The six [per-call bounds](#per-call-bounds) go on this table, since it is the
one making the calls. The judge is held to the same rules as any other:
one hop, no passthrough route, and none of the caller's credential slots travel
with the call. Because a delegated turn may consult it, inbound auth on such a
turn ranges over the overlay's targets and judge as well — so a delegated turn
that cannot authenticate is refused with zero judge calls. A `count_tokens`
probe makes none either, and answers from `default_target`'s first model.

## `[sentry]` (optional)

Opt-in error reporting to your own Sentry project. Off unless `dsn` is set; independent of `[otel]`. Reports gateway-owned diagnostics — fatal gateway startup/serve errors, panics, and `error`-level log events (`warn`/`info` as breadcrumbs, message only) — plus, unconditionally once `dsn` is set, an error/warning event whenever an upstream provider itself returns a failure: `error` for a 5xx response, `warning` for 429/529 (rate limit/overload), each tagged with `model`, `provider`, and `upstream_status` only. A streaming request that answers `200` and then fails mid-stream — an `event: error` frame, or the connection cut before a terminal event — also reports an event (`error`/`warning` respectively), tagged with `model`, `provider`, and `outcome`, and marks the request span `otel.status_code = error` (issue #287). A cut additionally carries a `cut_kind` tag saying which kind it was — `eof` (the body simply ended), `transport_error` (the body read failed), or `marker` (shunt had already detected the cut and synthesized a completion so the client stream stayed well-formed) — and every mid-stream event carries diagnostic context: how many SSE events and body bytes reached the client, the last event type seen, elapsed and time-to-first-token in milliseconds, and, for a `transport_error`, the upstream error's message (issue #310). These events are rate-limited in-process to a handful per minute, so one client retrying a cut stream cannot flood the project. Each combination of provider, model, and failure kind gets its own budget, and the three cut kinds count separately from each other and from an error-event failure — so a burst of `eof` cuts cannot hide a `transport_error` on the same model. A suppressed run is reported as a `suppressed_count` on the next event that gets through. The `shunt.stream_outcome` metric is never throttled. Request/response bodies, headers, and credentials are never sent. Metrics and tracing are each a further, separate opt-in.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `dsn` | — | Sentry project DSN. Empty disables; an invalid DSN is a startup error. Redacting secret — renders as `[redacted]` in diagnostic output; see [Secret references](#secret-references). |
| `environment` | — | Optional environment tag on reported events |
| `metrics` | `false` | Also send usage metrics — the gateway metric series documented in the OpenTelemetry guide (aggregates only) |
| `traces_sample_rate` | `0.0` | Also send performance traces: the per-request span becomes a Sentry transaction, head-sampled at this rate in `[0.0, 1.0]`. `0.0` sends no spans; out of range is a startup error. |
| `include_session_id` | `false` | Attach the client session id to request spans sent to Sentry |

## `[otel]` (optional)

Opt-in OpenTelemetry (OTLP/HTTP) export of traces, metrics, and logs to your own collector ([details](/guides/opentelemetry/)). Off unless `endpoint` is set; independent of Sentry.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `endpoint` | — | OTLP/HTTP base URL (e.g. `http://localhost:4318`); shunt appends `/v1/{traces,metrics,logs}`. Empty disables; a non-`http(s)` URL is a startup error. |
| `service_name` | `shunt` | `service.name` resource attribute (takes precedence over `OTEL_SERVICE_NAME`) |
| `environment` | — | Optional `deployment.environment.name` |
| `sample_ratio` | `1.0` | Head-based trace sampling in `[0.0, 1.0]`; out of range is a startup error |
| `traces` | `true` | Export the per-request `proxy_request` span |
| `metrics` | `true` | Export the gateway metric series documented in the OpenTelemetry guide |
| `logs` | `true` | Export `tracing` log events (stderr logs unaffected) |
| `include_session_id` | `false` | Attach the client session id to request spans |

## `[otel.headers]` (optional)

Extra headers on every OTLP request (e.g. a hosted-collector token). Merged under the standard `OTEL_EXPORTER_OTLP_HEADERS`. Each header value is a redacting secret and renders as `[redacted]` in diagnostic output; see [Secret references](#secret-references).

| Key | Meaning |
| :-- | :-- |
| any | Header name → value, e.g. `authorization = "Bearer <token>"` |

## Routing precedence

On a delegated turn, a matching `[models.subagents]` overlay → a matching `[models.router]` entry → a matching `[models.upstream_model]` entry → exact `[[routes]]` match → `[[route_prefixes]]` prefix match → `server.default_provider`.

The overlay comes first, and only for delegated work: a `Task` child's request
for an overlaid id is diverted to the overlay's target before the entry's own
router or map is consulted, while the parent's own turns — and `compaction` and
`auxiliary` — resolve the entry as if the table were absent. Everything below is
what they, and every id carrying no overlay, resolve through.

The router comes next because it is matched on the `[[models]]` entry itself: a
request for a router-backed id is answered by the router, which picks a tier and
resolves **that target** through the rest of the ladder — so the target, not the
router id, is what a `[[routes]]` entry should name. An exact entry naming the
router id is never consulted and warns at load. A `[[route_prefixes]]` entry is
unaffected: the router takes only its own id out of that prefix's reach.
