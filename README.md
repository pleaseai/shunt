# shunt

[![CI](https://github.com/pleaseai/shunt/actions/workflows/ci.yml/badge.svg)](https://github.com/pleaseai/shunt/actions/workflows/ci.yml)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/pleaseai/shunt?utm_source=badge)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=pleaseai_shunt&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=pleaseai_shunt)
[![codecov](https://codecov.io/gh/pleaseai/shunt/graph/badge.svg)](https://codecov.io/gh/pleaseai/shunt)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Greptile: The War on Bugs](https://www.greptile.com/badge.svg)](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source)

**English** · [한국어](README.ko.md) · [日本語](README.ja.md) · [简体中文](README.zh-CN.md)

> Shunt Claude Code to any model.

`shunt` is a spec-compliant [Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway-protocol): a transparent proxy that, for the **models you map**, diverts inference to another LLM provider at the **inference layer**. It routes by the request's `model` id — everything else passes through to Anthropic unchanged (the "shunt"; the fallback is configurable via `server.default_provider`).

The name is the mechanism: an electrical/railway *shunt* diverts a selected part of the flow onto a parallel path. Here, a mapped model's inference is diverted to another provider while Claude Code's tools and skills stay intact.

It ships with **OpenAI**, **ChatGPT/Codex** (reuse your subscription via `codex login`), **xAI** (API key), **Grok** (reuse your SuperGrok / X Premium+ subscription via `shunt login xai`), **Cursor** (reuse your subscription via `shunt login cursor`), **Gemini / Google Code Assist** (reuse your subscription via `~/.gemini/oauth_creds.json`; shunt uses a valid access token directly, while self-refresh requires `SHUNT_GOOGLE_CLIENT_ID` and `SHUNT_GOOGLE_CLIENT_SECRET`), and **Anthropic** passthrough built in — and any Anthropic-Messages-compatible backend (Kimi, DeepSeek, GLM, MiniMax, OpenRouter, Vercel AI Gateway, …) is one TOML table or YAML mapping away, no code changes.

> [!NOTE]
> `shunt` is pre-1.0 software under active development. Per [SemVer](https://semver.org/#spec), `0.x` releases may include breaking changes to configuration keys, the CLI, and behavior — check the [release notes](https://github.com/pleaseai/shunt/releases) before upgrading.

## Install

```bash
# Homebrew (macOS / Linux)
brew install pleaseai/tap/shunt

# Cargo, directly from the source repository
cargo install --git https://github.com/pleaseai/shunt
```

New versions are distributed through Homebrew and prebuilt binaries (macOS/Linux, arm64/x64) attached to each [GitHub release](https://github.com/pleaseai/shunt/releases); the crates.io package stops at the last version published there. See [Installation](https://shunt.dev/getting-started/installation/) for prebuilt-binary and from-source instructions.

### Run as a service (macOS/Homebrew)

```bash
brew services start shunt
```

Logs go to `$(brew --prefix)/var/log/shunt.log`. `brew services stop` sends `SIGTERM`, and shunt
drains in-flight requests before exiting; on Unix, Antigravity agent turns are terminated when
shutdown starts so their isolated process groups cannot hold the drain open. Editing the config file
afterwards doesn't need a restart — it [hot-reloads](docs/config-reload.md) automatically. Details:
[Running as a service](docs/running.md#run-as-a-background-service-homebrew).

## Quickstart

```toml
# shunt.toml — route a gpt-* id to your ChatGPT subscription
# [[routes]] is legacy for exact ids; prefer [models.upstream_model].
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"        # reuses `codex login`; use `openai` for OPENAI_API_KEY
```

```bash
codex login                                        # provider credential
shunt run                                           # -> listening on 127.0.0.1:3001

export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
claude                                              # /model -> pick gpt-5.6-sol
```

Unmapped models (all your `claude-*` ids) keep working exactly as before — shunt forwards them to Anthropic with your own credential. Full walkthrough: [Quickstart](https://shunt.dev/getting-started/quickstart/).

### Starter configuration

`shunt init` creates a commented `shunt.toml` in an existing directory. Keep the default passthrough starter, or scaffold ordered upstream presets without changing the fallback for unmapped models:

```bash
shunt init
shunt init --upstream codex --upstream kimi
```

### Agent-native setup blueprints

`shunt add` retrieves embedded Markdown implementation guides for coding agents. List the available upstream blueprints with `shunt add upstream`, or pipe one directly into an agent:

```bash
shunt add upstream kimi --print | claude
shunt add upstream https://provider.example/docs --print | claude
```

The command is offline and read-only: it prints guidance but never edits files, installs anything, or accesses the network. Use `shunt add provider <absolute-url>` when contributing support for a genuinely new provider protocol.

A shared deployment bounds in-flight inbound requests with `[server] max_concurrent_requests`
(default `1024`). Excess requests are shed immediately with `503` and `Retry-After: 1`, while
streaming requests keep their slot until the response body ends or the client disconnects. Set the
value to `0` to disable the limit; `/` and `/health` always remain available for liveness probes.
Shared gateways can also configure CIDR allow/deny rules, request/header/URL size limits, an
upstream response-header timeout that leaves streaming bodies uncapped, and independent device-flow
rate limits under `[server.access_control]`, `[server.limits]`, `[server.timeouts]`, and
`[server.rate_limits]`. Anthropic Messages and inbound Codex Responses request bodies default to a
32 MiB limit; raise `max_request_bytes` for larger file or image requests. Other gateway, admin,
telemetry, and analytics routes retain their endpoint-specific body limits.
See the [configuration reference](https://shunt.dev/reference/configuration/#server).

Secret-shaped values don't have to live in the config file as literals: any string can instead
be written as `${VAR}` (an environment variable, e.g. `"Bearer ${TOKEN}"`) or `${file:/abs/path}`
(a file's trimmed contents, as the field's entire value), resolved fresh on every config load —
including [hot reload](docs/config-reload.md), so a `${file:}`-backed secret can be rotated
without restarting shunt. Whether the new value then takes effect follows the field's own
reload behavior: `[sentry]` and `[otel]` are built once at startup, so rotating a secret in
those two sections still needs a restart. See [Config secret references](docs/config-secrets.md).

## Providers

A provider is either an ordered `[[upstreams]]` entry or a legacy `[providers.<name>]` TOML table (under YAML, an entry in the corresponding sequence or mapping). Two adapter kinds cover most upstreams: `kind = "anthropic"` (the upstream speaks Anthropic Messages; passed through, optionally with a different key) and `kind = "responses"` (the upstream speaks the OpenAI Responses API; shunt translates Anthropic Messages ⇄ Responses, streaming included). A third native kind, `kind = "cursor"`, bridges Cursor's ConnectRPC/protobuf AgentService so a Cursor subscription is reachable through the same Anthropic-Messages interface.

Ordered upstreams enable cross-provider failover. Declaration order is the attempt order; a model's `upstream_model` map selects the participating entries and maps its public id to each backend's id:

```toml
[server]
default_provider = "anthropic-primary"

[[upstreams]]
name = "anthropic-primary"
provider = "anthropic" # preset: kind, base_url, and default auth
auth = { mode = "claude_oauth", account = "primary" }

[[upstreams]]
name = "codex-fallback"
provider = "codex" # defaults to chatgpt_oauth

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic-primary = "claude-opus-4-8"
codex-fallback = "gpt-5.6-sol"
```

This chain tries `anthropic-primary` and then `codex-fallback`. `auth` accepts either a mode string or a map; `claude_oauth` and `chatgpt_oauth` maps can narrow credentials with `account = "name"` or `accounts = [...]`. Legacy `[providers.<name>]` remains supported and becomes implicit name-sorted upstreams. Do not declare both forms: mixing `[[upstreams]]` with `[providers.*]` is a configuration error. See the [configuration reference](https://shunt.dev/reference/configuration/) for presets, failure classes, and migration details.

**Built in:**

| Name | Kind | Auth | Backend |
| :-- | :-- | :-- | :-- |
| `anthropic` | `anthropic` | passthrough or Claude OAuth account pool | `api.anthropic.com` — forwards the caller's credential by default; `auth = "claude_oauth"` enables pooled subscription credentials |
| `openai` | `responses` | `OPENAI_API_KEY` | `api.openai.com/v1` |
| `codex` | `responses` | ChatGPT OAuth | `chatgpt.com/backend-api` — reuses `~/.codex/auth.json` (`codex login`) |
| `xai` | `responses` | `XAI_API_KEY` | `api.x.ai/v1` — the developer API, billed per token |
| `grok` | `responses` | xAI OAuth | `cli-chat-proxy.grok.com/v1` — the Grok CLI proxy; reuses `~/.shunt/xai-auth.json` (`shunt login xai` with a SuperGrok / X Premium+ subscription) |
| `cursor` | `cursor` | Cursor OAuth | `api2.cursor.sh` — reuses `~/.shunt/cursor-auth.json` (`shunt login cursor`) |
| `gemini` | `gemini` | Google OAuth | `cloudcode-pa.googleapis.com` — Google Code Assist backend; reuses `~/.gemini/oauth_creds.json` |
| `antigravity` | `antigravity` | None (local CLI) | Local `agy` binary — Google Antigravity backend; supports configured Gemini model slugs |

xAI may gate OAuth access by subscription tier — if `grok` returns 403, use the `xai` API-key provider instead. Details in [`docs/m6-xai-provider.md`](docs/m6-xai-provider.md).

**Antigravity is agentic.** The `antigravity` provider runs `agy` in agentic mode: the CLI uses its own tools to do the work and shunt streams its progress back as Anthropic SSE. A `tools` array on the request is therefore not forwarded and no `tool_use` block is ever returned — use the `gemini` provider when the client must drive the tool loop. Because a non-interactive run cannot answer a permission prompt, `agy` runs with `--dangerously-skip-permissions`, so **treat this provider as arbitrary code execution as the user running shunt**. Two settings bound it: `sandbox` (default `true`) passes `--sandbox`, which keeps reads and writes inside the workspace and is what actually contains the agent; `workspace_roots` decides only where it may *start*, gating the `Working directory:` path taken from the request's system prompt (client-controlled text) to canonicalized paths under roots you list. Keep the sandbox on and the bind on loopback. See the [providers guide](https://shunt.dev/guides/providers/).

**Anthropic multi-account.** An Anthropic provider with `auth = "claude_oauth"` can load explicit accounts from Claude Code credentials files or setup-token environment variables, or use private store-managed accounts created by `shunt login claude --name <name>`. Claude login has three modes: `--mode oauth` runs shunt's own refreshable OAuth flow (the TTY default), `--mode import` copies the current Claude Code login, and `--mode setup-token` creates a one-year inference-only token (`--long-lived` remains a deprecated alias). OAuth first uses an automatic `127.0.0.1` callback and falls back to hidden manual paste; use `--manual` to skip the callback. OAuth scope behavior differs by declaration form: legacy `[providers.*].accounts = []` scans the account store, while ordered `[[upstreams]]` must omit both `account` and `accounts` to scan the whole store; an explicit `accounts = []` is rejected. shunt keeps healthy `x-claude-code-session-id` sessions sticky, uses per-provider round-robin otherwise, and proactively rotates off a near-quota sticky account using model-aware 5-hour/weekly quota state before the wall when possible. The optional `[server.pool]` table tunes this selection (issue #135): soft per-window quota thresholds with per-account overrides (a low threshold marks a backup account), burn-rate–aware ordering and optional predictive avoidance of accounts projected to exhaust a window before it resets, plus per-account `priority` and `disabled` knobs. It can also enable `usage_refresh_seconds` polling of the Anthropic OAuth usage API for imported (refreshable) accounts, reconciling out-of-band consumption; polling is off by default. Setting `state_path` persists per-account quota to disk so a restart warm-starts from the last observed utilization instead of an empty pool (a best-effort cache — quota is re-derived from upstream regardless); persistence is off by default. Reactive handling of quota-rejected 429s, 401s, and 5xx responses remains the failover floor. Storm-control is a later follow-up. See the [how-to](https://shunt.dev/guides/anthropic-multi-account/), [configuration reference](https://shunt.dev/reference/configuration/), and [M8 behavior specification](docs/m8-anthropic-multi-account.md).

**Codex multi-account.** A `chatgpt_oauth` provider (the built-in `codex` provider or any `responses` provider using that auth mode) can likewise pool several ChatGPT accounts, provisioned by importing `codex login`'s credential with `shunt login codex --name <name>`, by running ChatGPT OAuth in the [admin web surface](https://shunt.dev/guides/admin-remote-provisioning/), or via explicit `credentials`/`token_env` account entries. OAuth scope behavior differs by declaration form: legacy `[providers.*].accounts = []` scans the account store, while ordered `[[upstreams]]` must omit both `account` and `accounts` to scan the whole store; an explicit `accounts = []` is rejected. shunt records the backend's `x-codex-*` 5-hour/7-day usage windows and feeds them into the same **quota-aware proactive selection** as the Claude pool — a near-quota sticky account yields before it returns a 429, with `[server.pool]` thresholds and burn-rate ordering applying — while cooldown-based reactive failover (429, 401, 5xx, credential-resolution failure) remains the safety floor. An opt-in `[server.pool] ramp_initial_concurrency` slow-start gate additionally protects a freshly selected account from being stampeded by concurrent in-flight requests after a failover. See the [how-to](https://shunt.dev/guides/codex-multi-account/) and [M10 behavior specification](docs/m10-codex-multi-account.md).

**Inbound Codex endpoint.** shunt can also run the other direction: an opt-in `[server.codex_endpoint]` table registers a raw OpenAI Responses passthrough (`/responses`, `/v1/responses`, `/backend-api/codex/responses`) so the **Codex CLI itself** can point its `base_url` at shunt and be load-balanced across the same ChatGPT/Codex OAuth account pool — a byte-for-byte relay, not the Anthropic-translating path above. It also accepts the CLI's two analytics paths as privacy-preserving discard sinks, recording only sanitized event-name counters and never forwarding the payload upstream. It is off by default; absent the table, none of those routes are registered. See the [how-to](https://shunt.dev/guides/inbound-codex-endpoint/) and [M11 behavior specification](docs/m11-inbound-codex-endpoint.md).

**Bounded upstream retry.** Transient upstream failures on a provider's single-credential path are retried with exponential backoff and randomized jitter, before any bytes reach the client (never mid-stream). Connection-level transport errors (connect reset/refused, timeout) always retry — nothing was accepted before they resolve. A transient response *status* (`429`/`502`/`503`/`504`/`529`, Anthropic's "Overloaded") is retried only on the idempotent Cursor path; the non-idempotent Anthropic Messages and single-credential Responses POSTs surface it immediately, since a response means the upstream may already have accepted a billable generation (issue #126). Other `4xx` never retry. It honors `Retry-After` (both delta-seconds and HTTP-date forms), is held off `count_tokens`, and is configurable per provider under `[providers.<name>.retry]` (on by default, conservative; set `max_retries = 0` to disable). The `claude_oauth`/`chatgpt_oauth` account pools use their own account-rotation failover instead. See the [configuration reference](https://shunt.dev/reference/configuration/#providersnameretry).

**Opt-in verify-only inbound JWT auth.** Add `[[server.auth.jwt]]` entries under `[server.auth]` to accept a JWT minted by your own identity provider as an inbound credential: shunt validates it against the issuer's JWKS and maps the verified claims to a caller identity, but **issues nothing** — no login flow, no session store, no signing secret, so it stays stateless across replicas. This is the third inbound option between a shared static token (no per-user identity) and `[server.gateway]` device login (per-user identity, but a signed-in gateway session gives up a set of client features, enumerated in the [M-A login note](docs/gateway-login.md)). The client is configured exactly as it is for a static token — a base URL plus a bearer — so it stays in first-party mode. Verification pins the algorithm from config (the token header's `alg` never selects it), requires `kid`, refetches an issuer's key set at most once per 60-second window on an unknown `kid`, and checks `exp`/`nbf`, `aud`, `azp`, `exp - iat` against `max_token_age_seconds`, `email_verified`, and an exact email-domain allowlist. An issuer whose key set is unreachable answers `503` rather than `401`, so an IdP outage is not reported as a bad credential. Off by default; with no entries, `[server.auth]` behaves exactly as before. See the [configuration reference](https://shunt.dev/reference/configuration/#serverauthjwt-optional) and [behavior specification](docs/inbound-jwt-auth.md).

**Opt-in Claude apps gateway login and policy.** Configure `[server.gateway]` to let managed Claude Code clients sign in through the OAuth device flow (`forceLoginMethod: "gateway"` + `forceLoginGatewayUrl`) instead of distributing one shared static token. Browser approval can use environment-backed static users or an allowlisted OIDC provider such as Google via `[server.gateway.oidc]`; both modes may be offered together. shunt serves OAuth discovery, browser approval, device/refresh grants, HS256 access JWTs, rotating opaque refresh tokens, and per-user `GET /managed/settings` with `ETag` caching, telemetry env push, and `availableModels` enforcement; the issued bearer gates `/v1/models` and inference routes whose selected provider injects a server-side credential, while passthrough providers remain open. It composes with `[server.auth]`. The surface is off by default. Refresh sessions persist across restarts by default (issue #194): `state_path` (default `~/.shunt/gateway-sessions.json`) stores refresh tokens as SHA-256 hashes in an atomically written, owner-only (0600 on Unix) file that is restored at boot, so users keep silently refreshing instead of re-running the browser flow. Set `state_path = ""` for memory-only sessions, where a restart invalidates refresh sessions; existing access JWTs remain valid until expiry, after which users must sign in again. Device grants are always memory-only. See the [setup guide](https://shunt.dev/guides/gateway-login/), [configuration reference](https://shunt.dev/reference/configuration/#servergateway-optional), [M-A login note](docs/gateway-login.md), [M-B managed-settings note](docs/gateway-managed-settings.md), and [M-C telemetry note](docs/gateway-telemetry.md).

**Opt-in gateway telemetry ingest.** A non-empty `[server.gateway.telemetry].forward_to` list does two things: it pushes the telemetry enable flag plus five `OTEL_*` environment values through managed settings (pointing every managed client's exporter at shunt), and it turns on verbatim relay for the inbound OTLP/HTTP routes those clients then post to — `POST /v1/metrics`, `POST /v1/logs`, and `POST /v1/traces`, registered with the rest of the `[server.gateway]` surface and gated by the same gateway bearer as `GET /managed/settings`. Payloads are relayed **verbatim** — the exact request bytes, with the inbound `content-type` and `content-encoding`, and never the client's `Authorization` header — so `application/x-protobuf` and `application/json` exporters both work and Claude Code's client-side attribution attributes survive. Each destination opts in per signal: `metrics` is on by default, while `logs` and `traces` are off because Claude Code log records and spans can carry command lines, prompts, and file paths. A signal with no opted-in destination is accepted and discarded, and relays run detached, so the client always gets an immediate `200` even when a collector is slow or down. Off by default; without destinations the routes accept and discard. See [Claude Code monitoring](https://code.claude.com/docs/en/monitoring-usage), the [configuration reference](https://shunt.dev/reference/configuration/#servergatewaytelemetry-optional), and the [M-C telemetry note](docs/gateway-telemetry.md).

**Opt-in admin web surface.** Configure `[server.admin]` to add an admin-authenticated **Accounts and usage** view that automatically observes supported host logins with recognizable masked identity and provider-native quota windows: Claude Code (credential file or macOS Keychain), Codex CLI (response-derived `x-codex-*` windows), Gemini CLI (all Code Assist model buckets), Kimi Code (weekly and 5-hour limits), Grok CLI (credit/product usage), and Cursor.app (billing-cycle, Auto + Composer, and named-model usage). Observation is read-only: shunt never refreshes, copies, or writes those source credentials; Cursor.app state is opened read-only and used only to derive an in-memory first-party web session. Claude usage is cached for 60 seconds, while the other provider readers run when the dashboard data is requested. Managed pool provisioning remains available under a collapsed advanced section for Claude and Codex accounts; those separately stored accounts are the credentials shunt owns and refreshes for load-balancing. The surface is off by default — absent the table, no `/admin*` routes are registered — and uses a credential separate from `[server.auth]`. To enable it in one step, run `shunt dashboard setup`: it generates an admin token into `~/.shunt/admin-token` (owner-only), wires it via `[server.admin].tokens_file` so no secret lives in the launch env, enables `[server.oauth_usage]`, and prints the dashboard URL — then restart shunt. Admin tokens can also come from `SHUNT_ADMIN_TOKENS`. See the [how-to](https://shunt.dev/guides/admin-remote-provisioning/) and [M9 design note](docs/m9-admin-surface.md).

**Opt-in client usage endpoint.** Configure `[server.usage]` to register a read-only `GET /usage` that returns a **sanitized, aggregated** view of the shared account pool's quota — per-window remaining headroom, reset time, and a coarse `ok`/`degraded`/`exhausted` status — so a non-admin client can anticipate throttling without the admin surface. It authenticates the same `[server.auth]` client token as `/v1/messages` (and requires that table), and never exposes account names, counts, priorities, `disabled` flags, or thresholds — the full per-account detail stays behind admin-only `/admin/pool`. Codex windows report `null` (no quota headers). Off by default; absent the table, the route is not registered. See the [configuration reference](https://shunt.dev/reference/configuration/#serverusage-optional) and [M12 design note](docs/m12-client-usage-endpoint.md).

**Opt-in Claude Code CLI native usage bars.** Configure `[server.oauth_usage]` to register `GET /api/oauth/usage`, the exact path the Claude Code CLI's own `Current session`/`Current week` usage bars fetch — so, when the CLI is pointed at shunt via `ANTHROPIC_BASE_URL`, its unmodified UI can render real, Claude-only, priority-tiered worst-case pool numbers instead of 404ing into an empty bar. **Precondition, partially verified:** it was confirmed **not** to fetch when `ANTHROPIC_AUTH_TOKEN` is set from `claude setup-token` or a shared-gateway client token — the two other documented shunt credential setups; that a full interactive `claude login` (subscription) session *does* fetch it is presumed from static analysis of the CLI binary and circumstantial UI evidence, not directly observed (a real subscription login could not be safely scripted in the recon environment). This is not "works out of the box" for every setup; see [M14 design note](docs/m14-oauth-usage-endpoint.md) for the full precondition evidence and its one unverified leg. Auth is bind-topology-gated (unauthenticated on loopback; a valid client token or gateway JWT — gated exactly as `/v1/messages`, not bare header presence — on a non-loopback bind, which also then requires `[server.auth]` or `[server.gateway]` to be configured). Off by default; absent the table, the route is not registered. See the [configuration reference](https://shunt.dev/reference/configuration/#serveroauth_usage-optional).

**Opt-in upstream status polling.** Configure `[server.status]` with one or more Statuspage `summary.json` sources to poll each provider's public status feed on an interval (5 minutes by default) and surface the last-observed indicator on an "Upstream status" strip in the admin dashboard and as the `shunt.upstream.status` gauge metric. It is strictly observation-only — nothing it reads ever feeds routing, failover, or pool/cooldown decisions. A source that fails to fetch, returns a non-2xx response, or reports an indicator shunt doesn't recognize is stored and reported as `unknown` rather than silently read as operational. Off by default; absent the table, no background polling starts. See the [configuration reference](https://shunt.dev/reference/configuration/#serverstatus-optional) and [design note](docs/upstream-status.md).

OpenAI's Thibault Sottiaux has publicly welcomed running Codex through other coding harnesses:

> Share the recipe. People want to know how to use GPT-5.6 Sol in CC. We don't discriminate on the harness. ([Source](https://x.com/thsottiaux/status/2075830097488249060))

He [followed up](https://x.com/thsottiaux/status/2076119366647894371) by walking through pointing Claude Code ("your orange crab") at GPT-5.6 Sol himself — exactly the inference-layer swap `shunt` performs, no separate app required.

That said, reusing your ChatGPT/Codex or SuperGrok subscription (or Kimi, Cursor, or other backends) from an unofficial client is your own call — a public welcome doesn't guarantee future policy or account enforcement. Use at your own risk.

**Cursor** works the same way — log in once and route a `cursor:*` model id:

```bash
shunt login cursor                                  # OAuth -> ~/.shunt/cursor-auth.json
```

```toml
# shunt.toml — route a cursor:<id> to your Cursor subscription
[[routes]]
model = "cursor:default"                             # "default" is the wire id for Auto; paid plans can use named ids
provider = "cursor"
```

The `cursor:` / `cursor-agent:` / `cursor-plan:` / `cursor-ask:` prefixes pick Cursor's agent mode (Agent / Plan / Ask); the suffix is the Cursor **wire** model id (Auto is `default`, not `auto`). The adapter streams assistant text and reasoning, bridges your client's tools as native Cursor MCP tool calls, and forwards inline images (issue #170). See [Providers → Cursor](https://shunt.dev/providers/cursor/) for details.

**Any Anthropic-compatible backend** is one table away — no code changes:

| Provider | `base_url` | Example model IDs |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k3[1m]`, `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | see [MiniMax docs](https://platform.minimax.io/docs/token-plan/claude-code) |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` |

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "MOONSHOT_API_KEY"

[[routes]]
model = "kimi-k3[1m]"
provider = "kimi"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

See [Providers](https://shunt.dev/guides/providers/) for the full list and per-provider notes.

## Documentation

Everything lives at **[shunt.dev](https://shunt.dev)**:

- [Quickstart](https://shunt.dev/getting-started/quickstart/) · [Why shunt?](https://shunt.dev/getting-started/why-shunt/) · [Providers](https://shunt.dev/guides/providers/) · [Configuration](https://shunt.dev/guides/configuration/) · [Troubleshooting](https://shunt.dev/reference/troubleshooting/)
- **For agents:** every page has a Markdown twin (append `.md` to any URL, or use the page's *Copy Markdown* / *Open in AI* buttons), and the site publishes [`/llms.txt`](https://shunt.dev/llms.txt), [`/llms-small.txt`](https://shunt.dev/llms-small.txt), and [`/llms-full.txt`](https://shunt.dev/llms-full.txt) per the [llms.txt spec](https://llmstxt.org/).

Design notes and milestone specs live in [`docs/`](docs/) (start with [`docs/implementation-plan.md`](docs/implementation-plan.md)). To route Claude Code to your ChatGPT/Codex subscription, see the [Codex configuration reference](docs/codex-configuration.md).

### Observability metrics

| Series | Type | Attributes | Meaning |
| :-- | :-- | :-- | :-- |
| `shunt.failover` | Counter | `provider`, `state` | Ordered-upstream failover transitions: `attempted`, `advanced`, or `exhausted`. |

See the [OpenTelemetry guide](https://shunt.dev/guides/opentelemetry/) for the complete metric table and export configuration.

## Why

Claude Code sends every turn to the Anthropic API. `shunt` sits in front (via `ANTHROPIC_BASE_URL`) and, for the models you map, diverts their inference to another provider (OpenAI, Codex/ChatGPT, …). Because routing happens at the HTTP/inference layer — not by handing the task off to a different CLI — the session keeps running inside Claude Code's harness: same tool loop, same preloaded skills, same bundled-script path resolution. Only token generation is outsourced.

Contrast with the alternative approach (handing a `subagent_type` off to another runtime like Codex CLI), which cuts higher in the stack and drops persona and preloaded skills.

### Per-model, not per-agent — and not a global swap

Selectivity is driven by the **`model` id on each request**, which Claude Code already lets you choose per context: the `/model` picker for the main session, a subagent definition's `model:` frontmatter, `CLAUDE_CODE_SUBAGENT_MODEL` for all subagents, or `ANTHROPIC_CUSTOM_MODEL_OPTION` to add a custom entry to the picker. So "divert only this agent / this session" is decided in Claude Code, and shunt just honors the model id it receives — no fragile per-agent system-prompt fingerprinting. Unlike global model-swap proxies, the main session can stay on Claude while only the models you name divert.

## Claude Code integration (official surface)

Claude Code exposes a **first-class gateway contract** behind `ANTHROPIC_BASE_URL` — `shunt` implements this rather than the fragile "hash the subagent's system prompt" heuristic that earlier Claude Code proxies rely on.

- [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol) — the API contract: endpoints, headers/body fields to forward vs consume, feature pass-through, and attribution. A running gateway serves the machine-readable spec at `GET /protocol`.
  - [Model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) — Claude Code queries `GET /v1/models?limit=1000` at startup (opt-in via `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) and adds returned models to the `/model` picker. By default, `auto_include_builtin_models = true` appends automatically discovered models after curated `[[models]]` entries, deduplicated by id; set it to `false` for a strictly curated list. Those models come from a live `GET /v1/models` against `server.default_provider` when it is Anthropic-kind, using that provider's authentication mode: `passthrough` forwards the caller's credential, so each caller sees its own entitled list; `api_key` uses the configured key; and `claude_oauth` uses the first resolvable, non-disabled account from the same effective account set as inference, including store-scanned accounts in `account_scope` order, without pool selection, cooldown, or quota accounting. The latter two modes expose a shared, gateway-credential-scoped catalog. shunt caches nothing and falls back to a builtin Claude catalog snapshot when the default provider is not Anthropic-kind, there is no credential, or the call fails (2 s cap). A curated entry may also include a `[models.upstream_model]` map (multi-entry with ordered `[[upstreams]]`, one entry under legacy providers), which makes the advertised id routable through the mapped upstream(s) and translates it to each mapped backend id without a separate `[[routes]]` entry. **Constraint:** entries whose `id` doesn't begin with `claude`/`anthropic` are ignored — non-Claude models must be aliased or added manually.
  - **System prompt attribution block** — Claude Code prepends a client-version + conversation fingerprint to the system prompt; stable for the conversation lifetime (v2.1.181+). `shunt` forwards it unchanged (never strips it — that's the developer's call via `CLAUDE_CODE_ATTRIBUTION_HEADER=0`).
- [Add a custom model option](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) — `ANTHROPIC_CUSTOM_MODEL_OPTION` adds a gateway-routed entry to the `/model` picker without replacing built-in aliases; the ID skips validation, so any string the gateway accepts works. **This is the primary way to select a non-Claude model** (e.g. `gpt-5.6-sol`), since discovery ignores ids that don't begin with `claude`/`anthropic`.
- **Tool search** (`ENABLE_TOOL_SEARCH`) — Claude Code defers MCP/LSP tool schemas and reveals them on demand via a `ToolSearch` tool, reclaiming context the model would otherwise spend on tools it never calls. Because shunt isn't a first-party Anthropic host, Claude Code keeps this **off** unless you opt in with `ENABLE_TOOL_SEARCH=true`. On the Codex/Responses path, `tool_search` under `[providers.<name>]` is a three-state setting: unset (the default, "auto") maps this onto the Responses API's own native client-executed `tool_search` protocol only for upstreams already known to implement it — the ChatGPT/Codex backend and `api.openai.com` — and keeps the #43 text shim for every other OpenAI-compatible endpoint (LiteLLM, vLLM, OpenRouter, a self-hosted proxy, ...); `tool_search = true` forces native once the upstream's flavor and model qualify (non-xAI/Grok, gpt-5.4+), letting you opt a verified custom endpoint in; `tool_search = false` always forces the shim, which adds each revealed tool to the cached `tools` prefix and invalidates it on every reveal. See the [Tool search](https://shunt.dev/guides/codex/#tool-search) guide.

**Design principle:** be a spec-compliant Anthropic-Messages gateway (`/v1/messages`, `/v1/models`, correct header/attribution pass-through), route by the request's `model` id, and translate Anthropic Messages ⇄ the OpenAI Responses API for mapped models — no prompt-shape heuristics that break on every Claude Code prompt change.

## Related work / prior art

**Claude Code–specific routers & proxies**

- [musistudio/claude-code-router](https://github.com/musistudio/claude-code-router) — the largest in this niche; use Claude Code as a foundation and decide how requests reach different models/providers.
- [1rgs/claude-code-proxy](https://github.com/1rgs/claude-code-proxy) — run Claude Code on OpenAI models.
- [fuergaosi233/claude-code-proxy](https://github.com/fuergaosi233/claude-code-proxy) — Claude Code → OpenAI API proxy.
- [seifghazi/claude-code-proxy](https://github.com/seifghazi/claude-code-proxy) — captures/visualizes in-flight Claude Code requests, with optional **per-agent** routing to other providers (the direct inspiration for `shunt`'s subagent-routing idea).
- [luohy15/y-router](https://github.com/luohy15/y-router) — a simple proxy enabling Claude Code to work with OpenRouter.
- [tingxifa/claude_proxy](https://github.com/tingxifa/claude_proxy) — Cloudflare Workers proxy translating Claude API requests to OpenAI format (Gemini, Groq, Ollama).
- [badlogic/claude-bridge](https://github.com/badlogic/claude-bridge) — use any model provider with Claude Code.
- [jimmc414/claude_n_codex_api_proxy](https://github.com/jimmc414/claude_n_codex_api_proxy) — cross-runtime router: proxies Anthropic **or** OpenAI API calls to the local **Claude Code or Codex** CLI (routes to the local CLI when the API key is all 9s, else the real cloud API). Note the inverse direction — routing cloud-API calls *to* local CLIs, rather than routing Claude Code agents *out* to cloud providers.
- [insightflo/chatgpt-codex-proxy](https://github.com/insightflo/chatgpt-codex-proxy) — Anthropic-compatible `/v1/messages` proxy that serves Claude Code inference from the **ChatGPT Codex backend** (uses a ChatGPT Plus/Pro subscription instead of an API key). Same inference-layer swap as `shunt`, targeting the Codex/GPT subscription backend while keeping Claude Code's UI and MCP tools.

**General AI gateways (adjacent infrastructure — possible backends)**

- [BerriAI/litellm](https://github.com/BerriAI/litellm) — SDK + proxy/AI gateway calling 100+ LLM APIs in OpenAI format, with cost tracking, guardrails, load balancing.
- [Portkey-AI/gateway](https://github.com/Portkey-AI/gateway) — fast AI gateway routing to 1,600+ LLMs with integrated guardrails.
- [maximhq/bifrost](https://github.com/maximhq/bifrost) — high-performance AI gateway with adaptive load balancing and 1000+ model support.
- [mazori-ai/modelgate](https://github.com/mazori-ai/modelgate) — open-source LLM gateway + MCP server (Go): RBAC/policy enforcement, multi-provider (OpenAI, Anthropic, Gemini, Bedrock, Azure, and local Ollama), an MCP gateway with semantic tool search, and semantic response caching.

### How `shunt` differs

Most Claude Code proxies above route **all** traffic to one alternative provider (a global model swap). `shunt`'s focus is **selective, per-model** diversion driven by the request's `model` id: keep the main session on Claude, and shunt only the models you name onto other providers — the switchboard/patchbay use case. Because Claude Code already lets you bind a model per context (main session, subagent `model:` frontmatter, `CLAUDE_CODE_SUBAGENT_MODEL`), that same selectivity reaches down to individual agents without shunt ever inspecting who the caller is.

## Contributing

Issues and PRs are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`AGENTS.md`](AGENTS.md) for build/test commands and conventions, and [`SECURITY.md`](SECURITY.md) for reporting vulnerabilities.

### Code review

Pull requests to `shunt` are reviewed by two AI code reviewers, both free for open source:

- [Greptile](https://www.greptile.com/?utm_source=oss_badge&utm_medium=readme&utm_campaign=greptile_for_open_source) — free for non-commercial MIT/Apache projects under its OSS program.
- [cubic](https://cubic.dev/) — free for public repositories.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

---

Made with Orca 🐋

- https://github.com/stablyai/orca
- https://www.onorca.dev/
