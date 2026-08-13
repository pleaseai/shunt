---
title: Configuration Reference
description: Every shunt.toml key — server, providers, routes, models.
---

The keys below are shown in TOML, but a config file may also be written in YAML (`shunt.yaml`/`shunt.yml`) — the schema is identical, only the syntax differs. See [Configuration](/guides/configuration/) for file locations, precedence, and an annotated example. Full template: [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).

## `[server]`

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `bind` | `127.0.0.1:3001` | Address shunt listens on |
| `default_provider` | `anthropic` | Provider for any model with no matching route |
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
| `upstream_ttfb_ms` | `120000` | Maximum wait for inference-upstream HTTP response headers; `0` disables. The response body then has no wall-clock cap, preserving long SSE streams. Returns `504 timeout_error` and hot-reloads |

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

A request whose whole route chain is `passthrough` skips this gate: such a route forwards the caller's own upstream credential, so the gateway lends them nothing. **`kind = "antigravity"` is exempt from that exemption** and is always treated as credential-injecting, whatever its `auth` says. The adapter ignores the caller's credential entirely and runs the operator's local `agy` with `--dangerously-skip-permissions`, so a passthrough Antigravity route would otherwise be unauthenticated local code execution as the user running shunt — sandboxed or not. Note this closes the exemption, it does not create a requirement: with neither `[server.auth]` nor [`[server.gateway]`](#servergateway-optional) configured, every route stays open, which is why an unsandboxed Antigravity provider is [refused outright off loopback](#providersname-legacy).

## `[server.admin]` (optional)

Presence of this table enables the admin web surface for browser account provisioning and account-pool health ([details](/guides/admin-remote-provisioning/)). When the table is absent, none of the `/admin*` routes are registered.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `header` | `x-shunt-admin-token` | Header carrying the admin token for API/curl calls |
| `tokens_env` | `SHUNT_ADMIN_TOKENS` | Env var holding comma-separated `name:token` pairs |
| `tokens_file` | _(unset)_ | Path to a file holding `name:token` pairs (one per line, or comma-separated), used when `tokens_env` is unset/empty |
| `session_ttl_secs` | `3600` | Browser session lifetime after login, in seconds |
| `pending_ttl_secs` | `600` | Time allowed to finish a started provisioning flow, in seconds |

Admin tokens can come from the environment or a file. The named environment variable must contain one or more credentials, for example `SHUNT_ADMIN_TOKENS="ops:<token>"`. Alternatively, set `tokens_file` to a path (`~` is expanded) and put the pairs there — this is what [`shunt dashboard setup`](/reference/cli/#shunt-dashboard-setup) writes to `~/.shunt/admin-token`, so no secret has to live in the launch environment. When both are set, a non-empty `tokens_env` wins. Startup fails closed if the table is present but neither source yields a valid credential (unset/empty/malformed, or an unreadable `tokens_file`).

Admin tokens are separate credentials from the client tokens configured under `[server.auth]`; do not reuse one credential for both surfaces.

### `[server.admin.oidc]` (optional)

Presence of this subtable adds an OIDC/SSO button to the admin browser login page. Admin tokens remain mandatory for API/curl access and as the token-form fallback. The allowlist is matched case-insensitively.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `public_url` | required | Externally reachable bare HTTPS origin for the admin surface; loopback HTTP is allowed. The redirect URI is `{public_url}/admin/oidc/callback` |
| `issuer` | required | OIDC discovery issuer. Must use HTTPS, except HTTP on loopback; a path is allowed |
| `client_id` | required | OIDC client id |
| `client_secret_env` | `SHUNT_ADMIN_OIDC_SECRET` | Env var holding the non-empty client secret |
| `allowed_domains` | `[]` | Case-insensitive email domains allowed to administer shunt |
| `allowed_emails` | `[]` | Case-insensitive full email addresses allowed to administer shunt |
| `scopes` | `openid email profile` | Scopes sent to the authorization endpoint |
| `authorization_endpoint` | discovery | Advanced authorization URL override; HTTPS or loopback HTTP only |
| `token_endpoint` | discovery | Advanced token URL override; HTTPS or loopback HTTP only |
| `userinfo_endpoint` | discovery | Advanced OIDC UserInfo URL override; HTTPS or loopback HTTP only |

At least one non-empty `allowed_domains` or `allowed_emails` entry is mandatory. Startup also fails closed for an invalid `public_url`, empty issuer/client id, or missing client secret. shunt accepts only a non-empty UserInfo email with `email_verified = true`. The browser flow uses PKCE and a `pending_ttl_secs`-bound, single-use state; callback/token/UserInfo failures produce generic browser messages without echoing provider input. The callback re-checks the current hot-reloaded allowlist before minting the same HttpOnly admin session cookie as token login, then redirects to the fixed `/admin` target.

For GitHub, SAML, or another non-OIDC provider, use an OIDC broker such as Dex; direct provider-specific OAuth2 integrations are out of scope.

## `[server.gateway]` (optional)

Presence of this table enables the [OAuth device-flow gateway login](/guides/gateway-login/) used by Claude Code's managed `forceLoginMethod: "gateway"`. When absent, shunt does not register `/.well-known/oauth-authorization-server`, `/oauth/device_authorization`, `/oauth/token`, `/device`, `/device/authorize`, `/device/callback`, `/managed/settings`, or the `POST /v1/metrics`, `POST /v1/logs`, and `POST /v1/traces` telemetry-ingest routes.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `public_url` | required | Externally reachable HTTPS origin used as the JWT issuer and base for advertised OAuth endpoints; `http` is accepted only for loopback |
| `jwt_secret_env` | `SHUNT_GATEWAY_JWT_SECRET` | Env var holding the HS256 signing secret (at least 32 bytes) |
| `users_env` | `SHUNT_GATEWAY_USERS` | Env var holding comma-separated `email:secret` approval users; optional when `[server.gateway.oidc]` is configured |
| `token_ttl_seconds` | `3600` | Access-token lifetime; returned as `expires_in` |
| `trust_forwarded_for` | `false` | Trust `X-Forwarded-For`/`X-Real-IP` as the `/device` rate-limit identity; enable only behind a trusted proxy that replaces client-supplied values |
| `state_path` | `~/.shunt/gateway-sessions.json` | File persisting refresh sessions across restarts; tokens are stored as SHA-256 hashes and written atomically with owner-only permissions (0600 on Unix). Set `""` for memory-only sessions (also the fallback when no home directory resolves) |

Startup fails closed when the URL is not a bare HTTPS origin (`http` is allowed only on loopback), the TTL is zero, the secret is missing or shorter than 32 bytes, or neither a valid static-user list nor a valid external IdP is configured. Static-user secrets may contain `:` because only the first colon separates the email and secret. Changes to the environment-backed secrets, users, and IdP configuration hot-apply on config reload; adding or removing the gateway table requires a restart because the route tree is fixed at boot.

### `[server.gateway.oidc]` (optional)

Presence of this subtable replaces or supplements the password approval form with an OIDC provider such as Google. An allowlist is always required and is matched case-insensitively.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `issuer` | required | OIDC discovery issuer. Must use HTTPS, except HTTP on loopback; a path is allowed |
| `client_id` | required | OIDC client id |
| `client_secret_env` | `SHUNT_GATEWAY_OIDC_SECRET` | Env var holding the non-empty client secret |
| `allowed_domains` | `[]` | Case-insensitive email domains allowed to approve a device |
| `allowed_emails` | `[]` | Case-insensitive full email addresses allowed to approve a device |
| `scopes` | `openid email profile` | Scopes sent to the authorization endpoint; custom values must include `openid` and `email` |
| `authorization_endpoint` | discovery | Advanced authorization URL override; HTTPS or loopback HTTP only |
| `token_endpoint` | discovery | Advanced token URL override; HTTPS or loopback HTTP only |
| `userinfo_endpoint` | discovery | Advanced OIDC UserInfo URL override; HTTPS or loopback HTTP only |

At least one non-empty `allowed_domains` or `allowed_emails` entry is mandatory. shunt accepts only a non-empty UserInfo email with `email_verified = true`. The browser flow uses a single-use ten-minute state and PKCE, and callback/token/UserInfo failures produce generic browser messages without echoing provider input. The redirect URI registered at the provider is `{public_url}/device/callback`. For GitHub, SAML, or another non-OIDC provider, use an OIDC broker such as Dex; direct provider-specific OAuth2 integrations are out of scope.

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
| `headers` | none | Extra request headers applied to every relay to this destination; a configured key replaces the forwarded value rather than duplicating the header. Names and values are validated at startup |
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

### `[server.gateway.admin]` (optional)

Presence of this table registers the spend-limit Admin API. The keys name environment variables containing comma-separated `id:key` pairs. Key values must contain at least 32 characters. Both ids and key values must be unique across the two variables.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `write_keys_env` | `SHUNT_GATEWAY_ADMIN_WRITE_KEYS` | Env var holding keys that can read, create, replace, and delete caps |
| `read_keys_env` | `SHUNT_GATEWAY_ADMIN_READ_KEYS` | Env var holding GET-only keys |
| `blocked_message` | unset | Accepted for future enforcement errors; stage 1 does not use it |
| `audit_retention_days` | `365` | Accepted for the later audit retention sweep |
| `spend_retention_months` | `13` | Accepted for the later spend retention sweep |
| `identity_retention_days` | `90` | Accepted for the later identity retention sweep |
| `group_limit_mode` | `min` | `min` or `max`; accepted for later group-limit resolution |
| `state_path` | `~/.shunt/gateway-spend.json` | Versioned JSON file containing caps and audit records; `""` selects memory-only storage |

Unlike the upstream gateway YAML examples, shunt does not place key values or `${VAR}` placeholders in the config file. Export pairs such as `SHUNT_GATEWAY_ADMIN_WRITE_KEYS="terraform:<32-or-more-character-key>"`. The state file is replaced atomically with private permissions after each mutation. When no home directory resolves, the default is memory-only. The state path is fixed at boot; configuration reloads do not change it.

### `[server.gateway.enforcement]` (optional)

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `fail_closed_on_error` | `false` | Accepted for the later enforcement stage. Setting it to `true` without `[server.gateway.admin]` fails configuration validation |

Stage 1 does not enforce caps on `/v1/messages`. It also does not implement usage metering, `/effective`, `/audit`, retention sweeps, or group scopes.

## `[server.codex_endpoint]` (optional)

Presence of this table enables an inbound OpenAI Responses passthrough so the **Codex CLI** can point its `base_url` at shunt and be load-balanced across a ChatGPT/Codex OAuth account pool ([details](/guides/inbound-codex-endpoint/)). When the table is absent, none of those routes are registered.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `provider` | `codex` | Configured upstream name to serve inbound requests; must use `auth = "chatgpt_oauth"` |

Registers `POST /backend-api/codex/responses`, `POST /responses`, and `POST /v1/responses` — all served by the named provider's account pool. When `[server.auth]` is configured they require a valid client token (like the other injected-credential routes); with no `[server.auth]` they are **open** to anyone who can reach them while still injecting the operator's Codex credential, so gate them on anything beyond loopback. Unlike `/v1/messages`, the request is not translated to or from Anthropic Messages; it is relayed to and from the upstream verbatim.

## `[server.usage]` (optional)

Presence of this table registers a client-facing `GET /usage` endpoint that returns a **sanitized, aggregated** view of the shared account pool's quota state, so a non-admin client can anticipate throttling without the admin surface ([endpoint details](/reference/endpoints/)). When the table is absent, the route is not registered.

The table has no keys today — presence alone opts in. It **requires [`[server.auth]`](#serverauth-optional)**: the endpoint identifies its caller by client token, so shunt fails startup if `[server.usage]` is set without inbound auth rather than serve pool telemetry unauthenticated.

`GET /usage` authenticates the same client token as `/v1/messages` (configured header, `x-api-key`, or `Authorization: Bearer`) and reports per-window remaining headroom (`1 - min(utilization)` across non-disabled accounts, i.e. the least reported utilization among non-disabled accounts — a pool-wide aggregate, not a prediction of which account the next request will actually route to), each window's reset, and a coarse `ok`/`degraded`/`exhausted` status. It never exposes account names, counts, priorities, `disabled` flags, thresholds, or per-account numbers — the full per-account detail stays behind the admin-only [`GET /admin/pool`](#serveradmin-optional). The Codex backend publishes no quota headers, so its windows report `null`.

## `[server.oauth_usage]` (optional)

Presence of this table registers `GET /api/oauth/usage` — the exact path Claude Code CLI's own native usage bars fetch — so the CLI's unmodified UI can show real pool numbers when pointed at shunt via `ANTHROPIC_BASE_URL` ([endpoint details](/reference/endpoints/), [M14 behavior specification](https://github.com/pleaseai/shunt/blob/main/docs/m14-oauth-usage-endpoint.md)). When the table is absent, the route is not registered.

The table has no keys today — presence alone opts in. Its auth model differs from `[server.usage]`: on a loopback `[server.bind]` the route is unauthenticated (the caller cannot have reached it off the operator's own machine); on a non-loopback bind it requires a **valid** credential — a configured client token or a valid gateway JWT, gated exactly as `/v1/messages` is (bare header presence is not accepted) — and shunt fails startup (`OauthUsageEndpointRequiresAuthOnNonLoopback`) unless [`[server.auth]`](#serverauth-optional) or [`[server.gateway]`](#servergateway-optional) is also configured. shunt also refuses to boot if a `claude_oauth` provider's `base_url` resolves to this gateway's own bind (`OauthUsageSelfPollLoop`) — otherwise the outbound usage poller could read back its own synthesized aggregate instead of Anthropic's real usage.

`GET /api/oauth/usage` reports only `claude_oauth`-provider accounts (never Codex/Cursor/Grok), and uses a routing-aware, priority-tiered worst case per window rather than `/usage`'s pool-wide least-utilized aggregate: within the lowest-`priority` tier of available accounts (falling back to the full non-disabled set when none are available), it reports the *maximum* utilization — the worst case among the accounts the next request can actually route to, not an optimistic pool-wide minimum. **This route only helps when the CLI itself is configured to call it**, which was verified to happen only for a full interactive `claude login` session, not `claude setup-token` or a shared-gateway client token — see the design note for the full precondition evidence.

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
| `usage_refresh_seconds` | disabled (`0`/absent) | Poll interval, in seconds, for `GET /api/oauth/usage`; a positive value below 60 is clamped up to a 60-second floor |
| `state_path` | unset | File the pool's per-account quota state is persisted to, so a restart warm-starts from the last observed utilization instead of an empty pool. Absent disables persistence (the default) |
| `ramp_initial_concurrency` | disabled (`0`/absent) | Storm control: initial concurrent-admission allowance for an account identity that just started taking traffic. `0` or absent disables admission gating |

For each window `X`, the effective soft threshold resolves as: account `threshold_X` → account `threshold` → `default_threshold_X` → `default_threshold` → `hard_threshold`, and is capped at `hard_threshold`. All thresholds are utilization fractions in `[0.0, 1.0]`; out-of-range values fail startup. The threshold and burn-rate knobs govern both pool families: the Anthropic pool from its `anthropic-ratelimit-unified-*` headers, and the Codex/ChatGPT pool from its `x-codex-*` 5-hour/weekly windows (Codex has no Fable-scoped `7d_oi` window, so `default_threshold_fable` is inert there). `usage_refresh_seconds` is Anthropic-only — Codex has no out-of-band usage API.

A positive `usage_refresh_seconds` additionally starts a background poller that reconciles Claude account-pool quota state against the Anthropic OAuth usage API ([details](/guides/anthropic-multi-account/#usage-api-reconciliation)); absent or `0` disables it (the default). Only imported (refreshable) `claude_oauth` accounts are polled — a long-lived `claude setup-token` or `token_env` account is skipped because the usage endpoint rejects a non-refreshable token. The poller reconciles the pool's header-derived 5h/weekly/Fable (`7d_oi`) quota state with authoritative usage, including out-of-band consumption of the same account outside shunt. The interval is fixed at boot; a config reload does not start, stop, or re-tune the poller.

`state_path` persists the pool's quota state (per-window utilization and reset, across every provider's accounts) to disk. Without it, a restart begins with an empty pool: every account looks unseen until its first post-restart response, which disables burn-rate avoidance and leaves `GET /usage` blank until traffic re-populates the pool. The file is a best-effort cache, not a source of truth — quota is re-derived from upstream responses regardless, so a missing, stale, or corrupt file only costs a cold start, never a boot failure. Writes use a private (`0600` on Unix) temp file, atomically rename it over the target, and happen on a background timer only when quota changed; failed writes retry on the next tick. Cooldowns are not persisted (they lapse on restart), and any restored window whose reset has already passed is dropped lazily by the first selection or snapshot after restore. The path is fixed at boot; a config reload does not start, stop, or re-point persistence.

A positive `ramp_initial_concurrency` enables **storm control** on every account pool: after a failover switch, concurrent in-flight requests would otherwise all land on the freshly selected account at once. With the gate on, an identity that just started taking traffic (fresh, back from a cooldown, or idle for 60 seconds) admits at most the configured number of concurrent requests; each successful response doubles the allowance (slow start), a failover-worthy failure restarts the ramp, and a denied request spills to the next account in selection order. The last remaining candidate is always attempted regardless of the gate, so gating can defer but never fail a request that an ungated pool would have served. Note this also means a pool whose accounts all resolve to a single upstream identity is effectively ungated: its only candidate is always the last candidate, so the setting only takes effect with two or more distinct account identities.

## `[server.status]` (optional)

Observation-only background polling of provider Statuspage `summary.json` endpoints, for visibility rather than decisioning: it never feeds routing, failover, or pool/cooldown behavior. It only updates a shared store surfaced by the `shunt.upstream.status` metric and the admin dashboard's "Upstream status" strip ([`GET /admin/status`](/reference/endpoints/)). When the table is absent, or `sources` is empty, the poller does not start.

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

`GET /admin/status` (admin-authenticated) returns each configured source's most recently observed indicator, description, incidents, and observed timestamp. A configured source whose first poll has not completed is returned as `unknown`; an unconfigured or empty `[server.status]` reports an empty `sources` list, which the dashboard reads as "hide this section" rather than rendering an empty table.

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
| `kind` | without a preset | `anthropic`, `responses`, `cursor`, `gemini`, or `antigravity`. The last two have no entry in the preset table below — the built-in `[providers.*]` tables of the same names are the separate legacy mechanism, not presets — so an ordered upstream on either must set `kind` explicitly. |
| `base_url` | without a preset | Upstream base URL. For `kind = "cursor"`, this is the login/token-refresh surface only; inference uses the fixed agent host `https://agentn.global.api5.cursor.sh`, overridable only with `SHUNT_CURSOR_AGENT_BASE_URL`. For `kind = "antigravity"` there is no upstream to address — the adapter runs a local `agy` binary and never reads this field — but a presetless upstream must still set it; any placeholder (the built-in provider uses `http://localhost`) will do. |
| `auth` | no | Auth mode string, or a mode-specific map. Defaults to the preset's auth, otherwise `passthrough`. |
| `effort`, `service_tier`, `count_tokens`, `websocket`, `tool_search`, `request_compression`, `retry` | no | Same per-upstream settings documented for legacy providers. Presets do not override `count_tokens`. `retry` is normalized for Cursor upstreams but does not apply to the Cursor streaming turn. |
| `workspace_roots`, `sandbox` | no | Same Antigravity controls documented for legacy providers, with the same defaults (`[]` and `true`). An ordered upstream needs them for the same reasons a `[providers.*]` entry does. |

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

A bare string such as `auth = "claude_oauth"` is shorthand for `auth = { mode = "claude_oauth" }`. `api_key` maps accept `env` (required unless the preset supplies it) and `header` (`bearer` by default, or `x_api_key`). `claude_oauth` and `chatgpt_oauth` maps may select `account = "name"` or `accounts = [...]`, but not both. `accounts` accepts bare store-entry names and full account tables; an explicitly empty `accounts = []` is rejected, while omitting both scope fields scans the whole store. If the ChatGPT store is empty, `chatgpt_oauth` retains its `~/.codex/auth.json` fallback. `passthrough`, `xai_oauth`, and `cursor_oauth` maps take only `mode`; unknown mode-specific keys are errors.

Do not combine `[[upstreams]]` with `[providers.*]` in the config file: startup fails when both file-layer declaration forms are present. Environment variables may override individual fields by normalized upstream/provider name under either form, using `SHUNT_PROVIDERS__<name>__<field>`. Declare the ordered `[[upstreams]]` array in the config file rather than trying to synthesize the whole array with one environment variable. Legacy `[providers.<name>]` remains supported and is normalized to implicit name-sorted upstreams. Because that form has no declared failover order, it supports only zero- or one-entry model maps; use `[[upstreams]]` before adding multiple entries to a model map.

### Failover behavior

For a multi-entry model map, shunt filters the declared upstream sequence to the names in the map. It advances after an upstream status `429`, `401`, `403`, `404`, or any `5xx`, and after a failure before upstream response headers arrive. Gateway-local errors that do not represent an upstream attempt, such as auth misconfiguration or adapter-owned validation/header construction errors, return immediately so failover does not mask the configuration problem. There is no failover after `2xx` headers have been returned, including a later streaming-body failure.

When the chain is exhausted, shunt returns the best relayed failure with preference `429` → `401`/`403` → `404` → other `5xx`. Pre-header failures are not remembered as best failures. If no relayed response was remembered, shunt returns a `502 api_error` with `all upstreams failed (N attempted)`.

For a `passthrough` upstream, the client's own `authorization` / `x-api-key` is forwarded on a failover attempt only when the **primary** route is itself `passthrough` and the attempt's destination origin matches that primary's. The credential is then the caller's own upstream credential, origin-specific to the primary, so a `passthrough` failover attempt on a **different** origin strips it and fails closed rather than replaying a host-specific token to another origin; a same-origin fallback (e.g. two passthrough entries on one host) still carries it. When the primary instead injects a credential (`api_key`/OAuth), the client headers are a gateway/client secret rather than an upstream credential, so every `passthrough` fallback strips them regardless of origin. `api_key`/OAuth upstreams inject their own server-side credential regardless of position.

Every proxied success or final failure carries `x-gateway-upstream` (selected upstream name), `x-gateway-model` (client-requested id), and `x-gateway-upstream-model` (mapped backend id). `count_tokens` uses only the first chain element and never fails over. `[server.codex_endpoint]` remains pinned to its configured upstream and does not participate in this chain.

### Migrating existing configurations

Existing configurations require **no action**. Legacy providers retain their routing and name-sorted selection behavior. Three additive or deliberate behavior changes apply on upgrade:

1. Legacy providers that resolve to the same physical OAuth account now share quota windows, health, cooldown, refresh locks, and in-flight admission state. The pool persistence key schema was version-bumped, so an existing `state_path` cache is ignored once and the pool has one cold start.
2. Every proxied response gains the three `x-gateway-*` metadata headers described above.
3. On the Anthropic Messages route (`/v1/messages`), a Claude or Codex OAuth pool of any size in which every attempt fails before response headers now returns `all upstreams failed (N attempted)` instead of its pool-specific `all Claude OAuth accounts failed before receiving an upstream response` or `all Codex OAuth accounts failed before receiving an upstream response` message. The separate `[server.codex_endpoint]` inbound path is unaffected and retains the Codex-specific message.

To opt into ordered failover, rewrite each `[providers.<name>]` table as a `[[upstreams]]` entry with the same name, fold `api_key_env`, `api_key_header`, and OAuth `accounts` into the `auth` map, arrange the entries by preference, and add each participating name to the model's `upstream_model` map.

The `kimi` preset reads `MOONSHOT_API_KEY`. Older examples that explicitly used `api_key_env = "KIMI_API_KEY"` continue to work in the legacy form; an explicit upstream map also preserves that name with `auth = { mode = "api_key", env = "KIMI_API_KEY" }`. Only users relying on the preset default need to export `MOONSHOT_API_KEY`.

## `[providers.<name>]` (legacy)

Each provider is a table under a name of your choosing. Built-ins (`anthropic`, `openai`, `codex`, `xai`, `grok`, `cursor`, `gemini`, `antigravity`) can be partially overridden — config maps deep-merge.

| Key | Values | Meaning |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` \| `cursor` \| `gemini` \| `antigravity` | Upstream protocol / adapter. `anthropic` = Messages API (passed through, optionally re-keyed); `responses` = Anthropic Messages translated to the OpenAI Responses API; `cursor` = the native Cursor ConnectRPC/protobuf AgentService adapter; `gemini` = Anthropic Messages translated to Gemini `generateContent`/`streamGenerateContent` on the Google Code Assist backend; `antigravity` = no upstream at all, running the local Antigravity CLI binary (`agy`) as a subprocess. |
| `base_url` | URL | Upstream base; shunt appends the endpoint path. For `kind = "cursor"`, this is the login/token-refresh surface only; it does not select the agent/inference host. |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `claude_oauth` \| `xai_oauth` \| `cursor_oauth` \| `google_oauth` \| `none` | `passthrough` forwards the client's own credential; `api_key` injects a key from `api_key_env`; `chatgpt_oauth` reuses `~/.codex/auth.json`; `claude_oauth` selects from explicit Anthropic accounts; `xai_oauth` reuses `~/.shunt/xai-auth.json` from `shunt login xai` (only sent to x.ai/grok.com hosts over HTTPS); `cursor_oauth` reuses `~/.shunt/cursor-auth.json` (`shunt login cursor`); `google_oauth` reuses the gemini CLI login in `~/.gemini/oauth_creds.json` and is valid only with `kind = "gemini"`; `none` sends no credential at all, for adapters with no upstream to authenticate against (`kind = "antigravity"`). |
| `api_key_env` | env var name | Where the key is read from, when `auth = "api_key"`. |
| `api_key_header` | `bearer` (default) \| `x_api_key` | Header the injected key is sent in. |
| `accounts` | array of account tables | Anthropic OAuth account pool. Valid only with `kind = "anthropic"` and `auth = "claude_oauth"`; see below. |
| `effort` | `low` … `max` | Optional default reasoning effort (`responses` providers). |
| `service_tier` | `fast` \| `priority` \| `flex` \| `default` | Optional default Codex "Fast" mode opt-in (`responses` providers) — sent as the Responses API `service_tier` field. `fast` normalizes to `priority`; `default` is a client-only sentinel that is never sent on the wire. Off by default. Withheld for the `xai`/`grok` flavors even when configured (xAI 400s on it). A route-level `service_tier` (including an explicit `default`) overrides this value — see below. See [Codex → Fast mode](/guides/codex/#fast-mode). |
| `count_tokens` | `tiktoken` (default) \| `estimate` | `responses` and `cursor` providers: local tiktoken count vs. `501 not_supported` fallback ([details](/guides/effort-and-context/#token-counting-count_tokens)). |
| `websocket` | `true` \| `false` (default) | Opt in to the Codex Responses WebSocket v2 transport (ChatGPT/Codex backend only; falls back to HTTP on any transport failure before the first event reaches the client, so it can never do worse than plain HTTP). |
| `tool_search` | unset ("auto", default) \| `true` \| `false` | Use the native client-executed `tool_search` protocol for Claude Code's tool search on a GPT-5.4+ model, gated on flavor (non-xAI/Grok). Unset defaults to native only for known-good hosts — the ChatGPT/Codex backend and `api.openai.com` — and the text shim everywhere else, including custom OpenAI-compatible endpoints (LiteLLM, vLLM, OpenRouter, self-hosted). Set `true` to opt a verified custom endpoint into native, or `false` to always force the shim. See [Codex → Tool search](/guides/codex/#native-protocol). |
| `request_compression` | `true` (default) \| `false` | zstd-compress the Responses **request** body (`content-encoding: zstd`, level 3), matching what the Codex CLI sends to the same backend. Effective only on the ChatGPT/Codex flavor (`auth = "chatgpt_oauth"`) — no other Responses upstream is verified to accept a compressed request body, so the flag is inert there. Set `false` to send plain JSON, e.g. behind a middlebox that mishandles compressed request bodies. |
| `retry` | sub-table | Bounded retry/backoff for supported transient upstream failures. On by default (conservative); see below. Normalized but inert for the Cursor streaming turn. |
| `workspace_roots` | array of paths (default `[]`) | `kind = "antigravity"` only. Roots inside which a prompt-supplied `Working directory:` may land. The system prompt is client-controlled and routinely quotes fetched documents, so it is prompt-injectable; a prompt-derived path is canonicalized (resolving symlinks and `..`) and honored **only** if it falls inside one of these roots. A path outside them is refused — the *path* is ignored, not the request, and the run falls back to the gateway's own working directory, exactly as it would had the prompt named no directory at all. Refusing the request instead would let anyone able to inject a line of system-prompt text fail every turn. Empty (the default) means no prompt-derived path is ever honored — only `SHUNT_AGY_WORKSPACE` or the gateway's own directory. |
| `sandbox` | `true` (default) \| `false` | `kind = "antigravity"` only. Runs the CLI with `--sandbox`, which keeps the agent's reads and writes inside the workspace. Print mode passes `--dangerously-skip-permissions` (it cannot service an approval prompt), so without the sandbox the agent has shell access and no workspace boundary — `workspace_roots` only changes where it *starts*. Set `false` only where unrestricted terminal access is genuinely needed and the caller is trusted. **Refused at startup when combined with a non-loopback `bind`**, which would hand arbitrary local execution to anyone who can post a Messages request. The adapter also enforces this rule per request against the listener bound at boot, so a hot reload cannot disable the sandbox while a public listener remains active. Restart shunt on a loopback bind before disabling it. |

### `[providers.<name>.retry]`

Bounded retry for **transient** upstream failures on supported single-credential calls: the `passthrough`/`api_key` Anthropic path and the single-credential Responses path (`api_key`, `xai_oauth`/Grok, and a `chatgpt_oauth` provider with no pooled accounts). It re-issues the request (full body, before any bytes reach the client) on connection-level transport errors (connect reset/refused, timeout). Transient response statuses are not retried on these non-idempotent creation POSTs because the upstream may already have accepted a billable generation. The current Cursor adapter's streaming turn is not wrapped in this retry layer, so its normalized `retry` table is inert and a pre-response connection failure surfaces directly. No supported path retries a `4xx` response, and retry never begins after response-body streaming starts.

Backoff is exponential with randomized (full) jitter, capped at `max_backoff_ms`. A server-supplied `Retry-After` takes precedence (both the delta-seconds and HTTP-date forms are honored); if it asks for longer than `max_backoff_ms`, the response is surfaced immediately rather than slept past budget. Retry is **held off `count_tokens`** regardless of this setting. The `claude_oauth` / `chatgpt_oauth` account pools drive their own account-rotation failover and are unaffected by this table.

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
| `name` | yes | Unique account label containing only lowercase ASCII letters, digits, and hyphens. A name-only entry resolves from the shunt-managed store. Returned to the client in `x-shunt-account`; avoid personal information. |
| `credentials` | one usable source | Path to a Claude Code `.credentials.json`-shaped file. `~/` is expanded. shunt refreshes near expiry and atomically writes refreshed tokens back. |
| `token_env` | one usable source | Environment variable holding a setup token. Used verbatim and not refreshable. Mutually exclusive with `credentials`. |
| `uuid` | no | Replaces an existing `metadata.user_id.account_uuid` in requests selected for this account. |
| `threshold` | no | Per-account soft quota threshold in `[0.0, 1.0]`, for every window without a per-window value. A low value marks a backup account that rotates out early. |
| `threshold_5h` / `threshold_7d` / `threshold_fable` | no | Per-window soft thresholds; each beats `threshold` for its window. See [`[server.pool]`](#serverpool-optional) for the full resolution order. |
| `priority` | no | Selection priority when the sticky account is unhealthy; lower is preferred, default `100`. Applies to Codex pools too. |
| `disabled` | no | `true` removes the account from selection entirely (kept in config and on the admin pool dashboard). Applies to Claude and Codex pools. |

A name-only entry reads `~/.shunt/accounts/claude/<name>.json`, created with `shunt login claude --name <name> --mode <mode>` (`<mode>` is one of `oauth`, `import`, or `setup-token`); the interactive CLI prompts for these three modes and recommends refreshable OAuth. `--long-lived` remains a deprecated alias for `--mode setup-token`. `SHUNT_CLAUDE_ACCOUNTS_DIR` overrides the store directory. An empty account list scans all valid store files. Refreshable OAuth/import files are updated in place when the provider rotates their refresh token, so each file must have one active owner: do not share or independently copy it across running shunt processes. Provision each process separately, or use a static setup token when appropriate. `claude_oauth` additionally requires an HTTPS `base_url` whose host is `anthropic.com` or a subdomain, preventing bearer leakage to another origin — except for loopback hosts (`localhost`, `127.0.0.1`, `[::1]`, …), which are exempt from both checks so a local debugging proxy or mock can be used over plain HTTP.

Account selection is session-sticky and quota-aware. On every upstream response handled by the `claude_oauth` account pool, shunt records `anthropic-ratelimit-unified-5h-utilization`, `anthropic-ratelimit-unified-7d-utilization`, `anthropic-ratelimit-unified-7d_oi-utilization`, their matching `*-reset` headers, the aggregate `anthropic-ratelimit-unified-status`, and the matching `*-status` headers for `5h`, `7d`, and `7d_oi`. A `rejected` 5-hour or shared `7d` status makes every model near quota. A `rejected` `7d_oi` status makes Fable requests near quota. Status rejection is evaluated independently of whether the matching utilization header is present. For utilization thresholds and headroom, Fable uses `7d_oi` when that utilization is available and shared `7d` otherwise; all other families, including Sonnet, use shared `7d`. The aggregate status is a compatibility fallback only when no per-window status has been recorded. Shared 5-hour utilization at or above its threshold or the model's governing weekly utilization at or above its threshold also makes an account near quota — the threshold is the built-in `0.98` unless tuned per account (`threshold*` above) or pool-wide ([`[server.pool]`](#serverpool-optional)). shunt keeps a healthy under-threshold sticky account, but rotates off a near-quota or cooled one and prefers available under-threshold accounts by `priority`, then by soonest governing-weekly reset (or, with `[server.pool]` configured, by largest burn-rate headroom — the projected time to threshold at the observed pace minus the time to reset; `burn_rate_avoidance = true` additionally treats a negative projection as near quota), then near-quota accounts (best headroom first when `[server.pool]` is set, so an all-near pool degrades to best-margin-first while accounts past `hard_threshold` still sort last), then cooled accounts. A 429 that rejects only `7d_oi` on a Fable request creates a Fable-only cooldown; for a non-Fable request, the same `7d_oi`-only rejection creates an account-wide cooldown; `5h` or shared `7d` rejection also creates an account-wide cooldown. Expired quota buckets clear automatically, and every non-disabled account remains selectable. Reactive failover remains active. Storm control for freshly switched account concurrency is available via [`[server.pool]` `ramp_initial_concurrency`](#serverpool-optional) (off by default).

See [Anthropic Multi-Account](/guides/anthropic-multi-account/) for the complete selection and failover behavior. The behavior reference is [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude); shunt has no runtime dependency on it.

## `[[routes]]`

Legacy exact-match routing entries — checked after a matching `[models.upstream_model]` entry:

> **Legacy:** For exact model ids, prefer a `[[models]]` entry with `[models.upstream_model]`; it both routes and advertises the id as one source of truth. `[[routes]]` remains supported indefinitely, but is no longer the recommended exact-routing form.

| Key | Required | Meaning |
| :-- | :-- | :-- |
| `model` | ✅ | The exact `model` id Claude Code sends |
| `provider` | ✅ | Configured upstream name |
| `upstream_model` | — | Rewrite the model id forwarded upstream |
| `effort` | — | Per-route reasoning-effort override |
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

Discovered models come from the live upstream list when shunt can get one. It issues `GET /v1/models` against `server.default_provider` when it is Anthropic-kind, using that provider's authentication mode. With `auth = "passthrough"`, shunt forwards the caller's credential, so each caller sees the list that credential is entitled to. If shunt consumed that credential as a `[server.auth]` client token or gateway-login bearer, it does not replay it upstream; discovery logs why no upstream credential remains and falls back to the snapshot. With `api_key`, shunt uses the configured key. With `claude_oauth`, it uses the first resolvable, non-disabled account from the same effective account set as inference, including store-scanned accounts in `account_scope` order. Discovery performs no pool selection, cooldown, or quota accounting. Those two gateway-owned modes therefore expose a shared credential-scoped catalog. shunt caches nothing. When the default provider is not Anthropic-kind, there is no credential, or the call fails or times out (2 s cap), shunt falls back to a builtin snapshot of the Claude catalog. Either way these ids need no dedicated `[[routes]]` entry — they resolve through your normal routing rules, falling back to `server.default_provider` when no `[[routes]]` or `[[route_prefixes]]` entry matches.

A curated entry can include `[models.upstream_model]` to advertise, route, and translate one id in the same declaration; this is the recommended form for exact-id routing instead of `[[routes]]`. With ordered `[[upstreams]]`, the map may contain one or more `upstream = "backend-id"` pairs and resolves to a failover chain in `[[upstreams]]` declaration order. With legacy `[providers.*]`, it must contain exactly one pair because that form has no declared order. For that id the map takes precedence over `[[routes]]`, `[[route_prefixes]]`, and `server.default_provider`; each upstream's default `effort` applies to its chain element. An empty map, an empty or whitespace-only upstream name or backend id, an unknown upstream, a same-id `[[routes]]` entry, a mapped id ending in `[1m]` or `[1M]`, or a duplicate `[[models]]` id where either entry has a map is a startup error. Clients strip the context-window hint before matching, so including it in a mapped id would make that entry unreachable. Pure map-less duplicate ids retain their previous behavior.

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

## `[sentry]` (optional)

Opt-in error reporting to your own Sentry project. Off unless `dsn` is set; independent of `[otel]`. Reports gateway-owned diagnostics — fatal gateway startup/serve errors, panics, and `error`-level log events (`warn`/`info` as breadcrumbs, message only) — plus, unconditionally once `dsn` is set, an error/warning event whenever an upstream provider itself returns a failure: `error` for a 5xx response, `warning` for 429/529 (rate limit/overload), each tagged with `model`, `provider`, and `upstream_status` only. A streaming request that answers `200` and then fails mid-stream — an `event: error` frame, or the connection cut before a terminal event — also reports an event (`error`/`warning` respectively), tagged with `model`, `provider`, and `outcome`, and marks the request span `otel.status_code = error` (issue #287). A cut additionally carries a `cut_kind` tag saying which kind it was — `eof` (the body simply ended), `transport_error` (the body read failed), or `marker` (shunt had already detected the cut and synthesized a completion so the client stream stayed well-formed) — and every mid-stream event carries diagnostic context: how many SSE events and body bytes reached the client, the last event type seen, elapsed and time-to-first-token in milliseconds, and, for a `transport_error`, the upstream error's message (issue #310). These events are rate-limited in-process to a handful per minute, so one client retrying a cut stream cannot flood the project. Each combination of provider, model, and failure kind gets its own budget, and the three cut kinds count separately from each other and from an error-event failure — so a burst of `eof` cuts cannot hide a `transport_error` on the same model. A suppressed run is reported as a `suppressed_count` on the next event that gets through. The `shunt.stream_outcome` metric is never throttled. Request/response bodies, headers, and credentials are never sent. Metrics and tracing are each a further, separate opt-in.

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `dsn` | — | Sentry project DSN. Empty disables; an invalid DSN is a startup error. |
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

Extra headers on every OTLP request (e.g. a hosted-collector token). Merged under the standard `OTEL_EXPORTER_OTLP_HEADERS`.

| Key | Meaning |
| :-- | :-- |
| any | Header name → value, e.g. `authorization = "Bearer <token>"` |

## Routing precedence

A matching `[models.upstream_model]` entry → exact `[[routes]]` match → `[[route_prefixes]]` prefix match → `server.default_provider`.
