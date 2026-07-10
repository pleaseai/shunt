# Running shunt

> How to build, configure, and run `shunt`, then point Claude Code at it.
> Based on the [Connect Claude Code to an LLM gateway](https://code.claude.com/docs/en/llm-gateway-connect)
> guide — shunt *is* the gateway you connect to.

`shunt` is a spec-compliant Claude Code LLM gateway. You run it locally, set
`ANTHROPIC_BASE_URL` so Claude Code talks to it instead of `api.anthropic.com`, and it
**diverts only the models you map** to another provider (OpenAI or a reused ChatGPT/Codex
subscription). Everything else passes through to Anthropic unchanged.

---

## 1. Prerequisites

- **Rust** (stable) with `cargo` — see `Cargo.toml`. Build with `cargo build`.
- **Claude Code** v2.1.129+ (only if you want [model discovery](#54-optional-model-discovery);
  the primary `ANTHROPIC_CUSTOM_MODEL_OPTION` path works on any recent version).
- A credential for whichever provider you map:
  - **OpenAI API key** for the `openai` provider, or
  - **A ChatGPT login** via the Codex CLI (`codex login`) for the `codex`/`chatgpt` provider.
- Your normal **Anthropic credential** (claude.ai login or an API key) — shunt forwards it
  through for every model you *don't* map.

---

## 2. Build

```bash
# Debug build
cargo build

# Release build (recommended for daily use)
cargo build --release   # -> target/release/shunt
```

You can also run straight from the source tree with `cargo run -- <args>` (examples below).

---

## 3. Configure

shunt loads configuration from, in increasing precedence:

1. Built-in defaults (all providers preconfigured — see `src/config.rs`).
2. A **TOML file**. With `--config <path>` that exact file is used (a missing
   file is an error). Otherwise shunt takes the first file found in:
   `./shunt.toml` → `$XDG_CONFIG_HOME/shunt/shunt.toml` (defaulting to
   `~/.config/shunt/shunt.toml`) → `$HOMEBREW_PREFIX/etc/shunt.toml`
   (defaulting to the `/opt/homebrew` and `/usr/local` prefixes). Boot
   logs report which file was loaded, or that defaults are in use.
3. **Environment variables** prefixed `SHUNT_`, using `__` for nested keys
   (e.g. `SHUNT_SERVER__BIND=0.0.0.0:3001`).

Because the defaults already define every provider, your `shunt.toml` only needs the parts you
want to change. Start from the template:

```bash
cp shunt.toml.example shunt.toml
```

### 3.1 Config reference

```toml
[server]
bind = "127.0.0.1:3001"        # address shunt listens on
default_provider = "anthropic" # provider for any model with no route (pass-through)

# Each provider is a [providers.<name>] table (see §3.3 for every key).
[providers.anthropic]
kind = "anthropic"             # forward Claude Code's own credential unchanged
base_url = "https://api.anthropic.com"

[providers.openai]
kind = "responses"             # translate Anthropic Messages -> OpenAI Responses
base_url = "https://api.openai.com/v1"
auth = "api_key"
api_key_env = "OPENAI_API_KEY" # env var the OpenAI key is read from
# effort = "high"              # optional default reasoning effort for this provider

[providers.codex]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"
auth = "chatgpt_oauth"         # reuses ~/.codex/auth.json
# effort = "high"

# --- Routing: how a request's `model` id picks a provider ---

# Exact match wins first. `upstream_model` and `effort` are optional overrides.
[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
# upstream_model = "gpt-5.6-sol"
# effort = "high"          # gpt-5.6 slugs also accept "max"

# Then prefix match.
[[route_prefixes]]
prefix = "gpt-"
provider = "openai"

# Optional: expose Claude-named aliases in the /model picker via discovery (§5.5).
# The id MUST start with "claude" or "anthropic" or Claude Code ignores it.
# [[models]]
# id = "claude-opus-via-codex"
# display_name = "Opus (via Codex)"

# Optional: error reporting to your own Sentry project. Off unless a DSN is
# set; nothing is ever sent by default. Only gateway-owned diagnostics are
# reported — panics and error-level log events, with warn/info as breadcrumbs.
# Request/response bodies, headers, credentials, and the host name are never
# sent: breadcrumbs keep only the log message (field values are stripped), and
# tracing spans never attach, so request-derived data has no path off the
# machine. An empty DSN (e.g. SHUNT_SENTRY__DSN="") disables reporting again;
# an invalid DSN is a startup error.
# [sentry]
# dsn = "https://<key>@<org>.ingest.sentry.io/<project>"
# environment = "home-lab"   # optional environment tag on events
# metrics = false            # (default) separate opt-in: also send usage metrics.
#                            # `shunt.requests` (count) and `shunt.latency` (ms
#                            # distribution; time to response headers for streams;
#                            # inference calls only, count_tokens excluded), tagged
#                            # provider, model (the requested id), and
#                            # `http.response.status_code`. Aggregates only — no
#                            # prompts, client names, or session ids.
```

**Routing precedence** (`src/routing.rs`): exact `[[routes]]` match → `[[route_prefixes]]`
prefix match → `server.default_provider`. A model with no match falls through to Anthropic.

### 3.2 Adding a provider

Providers are a **name → config map**, so a new upstream is just another `[providers.<name>]`
table — **no code change**. figment deep-merges the map, so a partial override of a built-in
(e.g. only `[providers.codex] effort = "high"`) keeps the rest of that provider's defaults, while
a brand-new table adds a provider. Every provider takes these keys:

| Key | Values | Meaning |
| :-- | :-- | :-- |
| `kind` | `anthropic` \| `responses` | Upstream protocol / adapter. `anthropic` = Messages API (passed through, optionally re-keyed); `responses` = Anthropic Messages translated to the OpenAI Responses API. |
| `base_url` | URL | Upstream base; Claude Code appends `/v1/messages`. |
| `auth` | `passthrough` \| `api_key` \| `chatgpt_oauth` \| `xai_oauth` | `passthrough` forwards the client's own credential (api.anthropic.com); `api_key` injects a key from `api_key_env`; `chatgpt_oauth` reuses `~/.codex/auth.json`; `xai_oauth` reuses the xAI subscription login in `~/.shunt/xai-auth.json` (`shunt login xai`; `kind = "responses"` only, base_url must stay on an https `x.ai` host). |
| `api_key_env` | env var name | Where the key is read from, when `auth = "api_key"`. |
| `api_key_header` | `bearer` (default) \| `x_api_key` | Header the injected key is sent in. |
| `effort` | `low`…`max` | Optional default reasoning effort (`responses` providers). |
| `count_tokens` | `tiktoken` (default) \| `estimate` | For `responses` providers: `tiktoken` computes a local count (o200k_base) and returns `{"input_tokens": N}`; `estimate` returns 404 so the client falls back on its own. See §4. |

Most third-party "use Claude Code with X" gateways are **Anthropic-Messages-compatible**: they are
`kind = "anthropic"` with `auth = "api_key"`, differing only in `base_url` and the key env var.
shunt injects the key and forwards the request. Ready-to-use entries (uncomment in
`shunt.toml.example`, set the env var, add a `[[routes]]` line):

| Provider | `base_url` | Example model IDs |
| :-- | :-- | :-- |
| Kimi (Moonshot) | `https://api.moonshot.ai/anthropic` | `kimi-k2.7-code` |
| DeepSeek | `https://api.deepseek.com/anthropic` | `deepseek-v4-pro`, `deepseek-v4-flash` |
| Z.ai (GLM) | `https://api.z.ai/api/anthropic` | `glm-5.2`, `glm-4.7` |
| MiniMax | `https://api.minimax.io/anthropic` | see [MiniMax docs](https://platform.minimax.io/docs/token-plan/claude-code) |
| Mimo (Xiaomi) | `https://api-mimo.mi.com/anthropic` | see [Mimo docs](https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode) |
| OpenRouter | `https://openrouter.ai/api` | `anthropic/claude-opus-4.8`, `~anthropic/claude-sonnet-latest` |
| Vercel AI Gateway | `https://ai-gateway.vercel.sh` | `anthropic/claude-opus-4.8` (accepts `x_api_key`) |

For example, to route Kimi's model through shunt:

```toml
[providers.kimi]
kind = "anthropic"
base_url = "https://api.moonshot.ai/anthropic"
auth = "api_key"
api_key_env = "KIMI_API_KEY"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

Then `export KIMI_API_KEY=…`, point Claude Code at shunt (§5.1), and select `kimi-k2.7-code`
(via `ANTHROPIC_MODEL` or the `/model` picker). Run `shunt check` to validate — it reports an
unknown provider in a route, a missing `api_key_env`, or a bad `base_url`.

### 3.3 Validate the config

```bash
cargo run -- check            # or: shunt check   /   shunt --check
# -> prints "config ok", or a specific error (bad bind address, unknown provider, …)
```

---

## 4. Run

```bash
# From the source tree
cargo run -- run                       # default subcommand is `run`, so `cargo run` also works
cargo run -- run --config ./shunt.toml

# From a release build
./target/release/shunt run
./target/release/shunt run --config /path/to/shunt.toml
```

On start it logs `shunt listening` with the bound address. Set log verbosity with `RUST_LOG`,
e.g. `RUST_LOG=shunt=debug cargo run -- run`.

### Endpoints served

| Method | Path                          | Purpose                                             |
| :----- | :---------------------------- | :-------------------------------------------------- |
| `HEAD` | `/`                           | Liveness probe                                      |
| `GET`  | `/`                           | Human-readable landing (version + endpoint list)    |
| `GET`  | `/health`                     | Healthcheck — `{"status":"ok","version":"x.y.z"}`   |
| `GET`  | `/v1/models`                  | Model discovery (returns your `[[models]]` entries) |
| `POST` | `/v1/messages`                | Inference — routed per the request's `model` id     |
| `POST` | `/v1/messages/count_tokens`   | Token counting (see below)                          |

`GET /` and `GET /health` stay open even when `[server.auth]` is enabled (healthcheck tools
usually cannot attach tokens) and expose nothing sensitive — only status, version, and the
already-public endpoint list.

**`count_tokens`:** for an **Anthropic-routed** model shunt passes the request through to the
upstream's `count_tokens` endpoint (exact counts). For a **`responses`-routed** model (codex/OpenAI)
there is no equivalent upstream endpoint, so the provider's `count_tokens` setting decides:

- `count_tokens = "tiktoken"` (default) — shunt computes the count locally with tiktoken's
  `o200k_base` encoder and returns `{"input_tokens": N}`. o200k_base is the GPT-family encoder, so
  for responses-routed models the text count is near-exact, though it can't see the backend's
  image/tool-schema encoding or cache accounting. Each count is answered in-process (~ms), which
  matters because Claude Code's `/context` issues one `count_tokens` call **per displayed item**
  (system-prompt section, memory file, agent, deferred tool, …) — 30–50 calls per invocation.
- `count_tokens = "estimate"` (opt-in) — shunt returns **404**, which the
  [gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol) explicitly allows for an
  absent endpoint. Note what Claude Code actually does then: the main-loop context bar estimates
  locally, but `/context` re-runs **every** category count against Haiku over the network — slow,
  and silently reported as 0 tokens when no Anthropic credential is available. Use it only if you
  want shunt to carry no tokenizer.

Either way the request never reaches the responses adapter, so a count request is never turned into
(and billed as) a full inference call. Opt out per provider:

```toml
[providers.codex]
kind = "responses"
count_tokens = "estimate"
```

---

## 5. Connect Claude Code

### 5.1 Point Claude Code at shunt

Set the base URL to your running gateway (default bind `127.0.0.1:3001`). Set it in your shell,
or persist it in a [settings file](https://code.claude.com/docs/en/settings) `env` block.

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
```

Or in `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:3001"
  }
}
```

Keep your existing Anthropic credential (claude.ai login, or `ANTHROPIC_API_KEY` /
`ANTHROPIC_AUTH_TOKEN` — §5.2 covers which to choose). shunt **forwards it unchanged** to
`api.anthropic.com` for every model you didn't map, so unmapped models keep working exactly as
before. Provider credentials for mapped models are injected by shunt itself (§5.3) — Claude Code
never sends them.

### 5.2 Choose the Anthropic credential

The credential Claude Code sends to shunt plays two roles: it authenticates **Claude passthrough
models** (forwarded unchanged to `api.anthropic.com`), and it **gates model discovery** — Claude
Code only issues the `GET /v1/models` request (§5.5) when `ANTHROPIC_AUTH_TOKEN`, an API key, or
an `apiKeyHelper` is set; under a plain claude.ai login it sends nothing. Mapped models (`gpt-*`
etc.) are unaffected either way — shunt injects the provider credential for them (§5.3).

| Credential | Token refresh | Discovery (§5.5) | Claude passthrough | Billing |
| :-- | :-- | :-- | :-- | :-- |
| claude.ai OAuth **login** only | Claude Code refreshes it automatically | ❌ never fires | ✅ | subscription |
| `ANTHROPIC_AUTH_TOKEN=sk-ant-oat…` (`claude setup-token`) — **recommended** | none needed (one-year token) | ✅ | ✅ | subscription |
| `apiKeyHelper` = `shunt token` (reuse the live login) | the helper refreshes it | ✅ | ✅ | subscription |
| `ANTHROPIC_AUTH_TOKEN=<real API key>` | none needed | ✅ | ✅ | **API (not subscription)** |

(A dummy value like `sk-dummy` satisfies the discovery gate but breaks passthrough — it is
forwarded to Anthropic and returns 401.)

**Prefer `claude setup-token`.** It mints a **one-year** OAuth token
([authentication docs](https://code.claude.com/docs/en/authentication#generate-a-long-lived-token)),
so nothing needs refreshing, and one value covers both roles — it satisfies the discovery gate
*and* authenticates Claude-passthrough models on your subscription:

```bash
claude setup-token                        # browser sign-in → prints sk-ant-oat…
export ANTHROPIC_AUTH_TOKEN=sk-ant-oat…   # or persist it in a settings `env` block
```

The login-only row works because Claude Code keeps refreshing its own login while no gateway
credential is set — you only lose discovery (use `ANTHROPIC_CUSTOM_MODEL_OPTION`, §5.4, instead).
The trap sits in between: **once a gateway credential is active, Claude Code stops refreshing the
login**, so the short-lived access token inside `~/.claude/.credentials.json` (macOS: Keychain)
expires within hours and a helper that just *reads* that file breaks. Refreshing it manually is
discouraged — `platform.claude.com/v1/oauth/token` is aggressively rate-limited/WAF-guarded (a
single stray call can return `429`), and it rewrites your live login file. To reuse the live
subscription login instead of minting a separate token, use the built-in `shunt token` helper,
which refreshes it safely.

#### The `shunt token` credential helper

`shunt token` prints a Claude subscription OAuth token to **stdout** (logs go to stderr), so it
can be wired straight into Claude Code's `apiKeyHelper`. It has two modes:

- **Static** — if `SHUNT_GATEWAY_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN` is set, it echoes that value
  unchanged. Point it at a `claude setup-token` value and nothing is ever refreshed.
- **Auto-refresh** — otherwise it reads `~/.claude/.credentials.json`
  (override with `CLAUDE_CREDENTIALS`), returns the `claudeAiOauth` access token, and when it is
  within 5 minutes of `expiresAt` refreshes it against `platform.claude.com/v1/oauth/token` (the
  same grant Claude Code uses), then writes the new token back **atomically at `0600`, preserving
  every other field**. Refresh happens only on actual expiry, to respect the endpoint's rate limit.

Wire it up via `apiKeyHelper` in `~/.claude/settings.json` (or the project `.claude/settings.json`):

```json
{
  "apiKeyHelper": "/path/to/shunt token"
}
```

Claude Code calls the helper for its gateway credential, so discovery fires; `SHUNT_GATEWAY_TOKEN`
(static) vs. no override (auto-refresh) selects the mode. The refresh path touches your real login
file, so the static + `setup-token` route stays the simplest and safest default.

### 5.3 Provide the mapped provider's credential (to shunt, not Claude Code)

- **OpenAI provider:** export the key named by `api_key_env` (default `OPENAI_API_KEY`) in the
  environment shunt runs in. shunt also reads a key from `~/.codex/auth.json` when it's in
  `ApiKey` mode.
  ```bash
  export OPENAI_API_KEY=sk-...
  ```
- **Codex / ChatGPT provider:** log in with the Codex CLI once; shunt reads and auto-refreshes
  `~/.codex/auth.json`.
  ```bash
  codex login
  ```
  If the file is missing/expired, shunt returns an `authentication_error` telling you to run
  `codex login`.

#### xAI Grok — API key or subscription OAuth

> **⚠️ Experimental — not yet verified against the live xAI API.** Implemented from the
> reference clients (Hermes, OpenCode) and covered by unit tests with mocked endpoints;
> it has not been exercised with a real SuperGrok account or `XAI_API_KEY` yet. Expect
> rough edges and report issues.

shunt ships a built-in `xai` provider (`kind = "responses"`, `base_url = https://api.x.ai/v1`).
It defaults to the **API-key** path; flip it to `xai_oauth` to reuse a **SuperGrok / X Premium+**
subscription. Full spec: [`m6-xai-provider.md`](m6-xai-provider.md).

**API key** (default) — just export the key and add routes:

```toml
# built-in defaults already define [providers.xai]; you only add routes
[[routes]]
model = "grok-build-0.1"    # flagship coding model
provider = "xai"

[[routes]]
model = "grok-4.3"
provider = "xai"
```

```bash
export XAI_API_KEY=xai-...
```

**Subscription OAuth** — flip the provider's `auth` and log in once with the device-code flow:

```toml
[providers.xai]
auth = "xai_oauth"          # reuse the SuperGrok / X Premium+ login instead of a key
# base_url stays https://api.x.ai/v1 — shunt refuses xai_oauth on a non-x.ai or non-https host

[[routes]]
model = "grok-build-0.1"
provider = "xai"

[[routes]]
model = "grok-4.3"
provider = "xai"
```

```bash
shunt login xai            # prints a URL + code; approve in any browser
```

`shunt login xai` runs the RFC 8628 device-code flow: it prints a verification URL and short
code, you approve in a browser (on any device — no loopback port needed, so it works over
SSH/VPS/Docker), and it saves `~/.shunt/xai-auth.json` (override with `SHUNT_XAI_AUTH_FILE`).
shunt refreshes the token automatically. A **403** on refresh means the account isn't entitled to
xAI API access (a subscription-tier gate) — shunt says so distinctly and points you at the
`XAI_API_KEY` path rather than telling you to re-login; a **400/401** tells you to run
`shunt login xai` again.

> **Reasoning effort is opt-in for grok.** Several grok models reject `reasoning.effort` with a
> 400, so shunt sends the reasoning dial only when an effort was explicitly chosen — an `effort` on the route or provider, or a per-request `output_config.effort` from the client
> (e.g. `effort = "high"` under `[providers.xai]` or a `[[routes]]` entry). Without it, grok
> reasons on its own and shunt sends no `reasoning` object.

### 5.4 Select a mapped model (primary path)

Claude Code's model-discovery only honors ids beginning with `claude`/`anthropic`, so for
OpenAI/Codex ids (`gpt-*`) use `ANTHROPIC_CUSTOM_MODEL_OPTION` — it adds a picker entry whose id
skips validation:

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
```

Then pick it from `/model` in Claude Code. That id is what shunt routes on, so it must match a
`[[routes]]`/`[[route_prefixes]]` rule in your config.

> **Model slugs:** the ChatGPT-account Codex backend **rejects** `gpt-*-codex` slugs (e.g.
> `gpt-5.2-codex`) — it only accepts the account's live-entitled slugs. The authoritative catalog
> of Codex slugs (and the reasoning levels each accepts) is openai/codex's
> [`codex-rs/models-manager/models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json).
> The current listed slugs are **`gpt-5.6-sol`**, **`gpt-5.6-terra`**, **`gpt-5.6-luna`** (latest,
> frontier), and **`gpt-5.5`** / **`gpt-5.4`** / **`gpt-5.4-mini`** / **`gpt-5.2`**; older accounts
> may only be entitled to the earlier ones. Use `upstream_model` in a route, or pass an entitled
> slug via `ANTHROPIC_CUSTOM_MODEL_OPTION`. See [`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md) §0.

> **Client-version gating:** some slugs additionally carry a `minimal_client_version` (e.g.
> `gpt-5.6-luna` requires ≥ 0.144.0) and the backend answers **`Model not found <slug>`** — not
> an entitlement error — when the request's client identity is missing or too old. The gate keys
> on the `originator` + `version` headers ([openai/codex#31967](https://github.com/openai/codex/issues/31967)).
> shunt therefore sends the Codex CLI identity headers (`originator: codex_cli_rs`,
> `version`, and a matching `user-agent`) on ChatGPT OAuth requests, **pinned to
> openai/codex rust-v0.144.1**. If a future slug demands a newer client, bump the pinned
> version in `src/adapters/responses.rs` (`CODEX_USER_AGENT` / `CODEX_CLIENT_VERSION`).

Per-context selection also works via Claude Code's own knobs — a subagent's `model:`
frontmatter, or `CLAUDE_CODE_SUBAGENT_MODEL` for all subagents — so you can divert only one
agent while the main session stays on Claude.

### 5.5 (Optional) Model discovery

Discovery (`GET /v1/models`) can populate `/model` automatically — **but Claude Code ignores
any id that doesn't begin with `claude`/`anthropic`** ([protocol
reference](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)). So a `gpt-*`
id is dropped client-side no matter what; discovery is only useful when you expose a
**Claude-named alias** that a `[[routes]]` entry rewrites to the real upstream slug:

```toml
[[models]]
id = "claude-gpt-5.6-sol-via-codex"     # must begin with claude/anthropic
display_name = "GPT-5.6-Sol (via Codex)"

[[routes]]
model = "claude-gpt-5.6-sol-via-codex"  # the alias Claude Code sends
provider = "codex"
upstream_model = "gpt-5.6-sol"          # real slug forwarded to the ChatGPT backend
```

Then enable discovery (Claude Code v2.1.129+) and restart shunt + Claude Code:

```bash
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
```

The alias appears in `/model` labeled *From gateway*; selecting it sends
`claude-gpt-5.6-sol-via-codex`, which shunt routes to `codex` and rewrites to `gpt-5.6-sol`. Discovery
fails **silently** (3-second timeout, any redirect counts as failure) and falls back to the
cached/built-in list — run `claude --debug` and look for `[gatewayDiscovery]` lines to confirm
it ran. For `gpt-*` ids without an alias, use `ANTHROPIC_CUSTOM_MODEL_OPTION` (§5.4) instead.
See [`m3-discovery.md`](m3-discovery.md).

> **Discovery needs a gateway credential — a claude.ai OAuth *login* alone won't trigger it.**
> Claude Code only issues the `/v1/models` request when `ANTHROPIC_AUTH_TOKEN`, an API key, or an
> `apiKeyHelper` is set; under a plain Max/Pro subscription login it sends nothing (no request
> reaches shunt, no cache is written) even with the flag on. Verified: shunt served `/v1/models`
> correctly, but the request never left Claude Code until a token was set. See §5.2 for choosing
> the credential — `claude setup-token` is the recommended route.

### 5.6 Reasoning effort

Claude Code's effort level (`/effort`, the `/model` slider, `--effort`, or
`CLAUDE_CODE_EFFORT_LEVEL`) is sent as the `output_config.effort` request field, and shunt maps
it to the Responses `reasoning.effort` for mapped models:

| Claude Code effort | → `reasoning.effort` |
| :-- | :-- |
| `low` / `medium` / `high` / `xhigh` | passthrough |
| `max` | passthrough on models that accept it (the **gpt-5.6** family), else folded to `xhigh` |

Which reasoning levels a Codex slug accepts is listed per-model in openai/codex's
[`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json)
(`supported_reasoning_levels`): `gpt-5.6-sol`/`-terra`/`-luna` accept up to `max` (sol/terra even
`ultra`, which Claude Code never sends), while `gpt-5.5`/`5.4`/`5.2` cap at `xhigh`. shunt folds
`max → xhigh` only for slugs that don't support it.

**For a custom gateway id like `gpt-5.6-sol` you must set `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`** —
otherwise Claude Code omits `output_config.effort` for model ids it doesn't recognize as
effort-capable, and shunt falls back to `medium`.

```bash
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
```

Precedence in shunt: a config `route.effort` / `[providers.*].effort` override wins first;
otherwise the request's `output_config.effort` is honored; otherwise `thinking.enabled → high`,
then a model-name suffix (`-xhigh`/`-high`/`-medium`/`-low`, with `-spark` treated as `-low`), else `medium`.

### 5.7 (Optional) Attribution block

Claude Code prepends an attribution line to the system prompt
(`x-anthropic-billing-header: cc_version=…; cc_entrypoint=cli;`). Anthropic strips it before
processing, but shunt forwards it unchanged, so a mapped provider such as the ChatGPT Codex
backend receives it as the first line of `instructions`. It's harmless (requests still succeed)
but meaningless noise for a non-Anthropic model. To drop it:

```bash
export CLAUDE_CODE_ATTRIBUTION_HEADER=0
```

This is global, so it also removes attribution from any Anthropic-passthrough traffic (used for
cost tracking) — which is fine when you're routing to another provider.

### 5.8 Context / usage display for mapped models

Claude Code's statusline and prompt footer compute the **context indicator locally** from the
assistant message's token `usage` (`input_tokens + cache_read + cache_creation`) divided by the
model's context-window size — no server field controls it. Two consequences for models routed to
a `responses` provider (codex/OpenAI):

- **Token count (the numerator) is accurate.** shunt reads `input_tokens` (and cached tokens) from
  the Responses `usage` and forwards them in the Anthropic `message_delta`, so the bar fills as the
  conversation grows. (The OpenAI `input_tokens` total includes cached tokens; shunt peels the
  cached part into `cache_read_input_tokens`, preserving the total.)
- **The window (the denominator) defaults to a fixed 200k for unmapped ids.**
  `getContextWindowForModel` returns `200_000` for any model id it doesn't recognize, and its
  accurate per-model lookup (`max_input_tokens` from the gateway's `/v1/models`) is **disabled
  unless the base URL is `api.anthropic.com`** — so a gateway can't set it. A model with a larger
  real window (e.g. `gpt-5.6-sol` at 372k) therefore shows a **conservative, over-reported**
  percentage. This only makes Claude Code's auto-compact trigger a little early; it is otherwise
  harmless.

The 200k default **can be overridden client-side** with `CLAUDE_CODE_MAX_CONTEXT_TOKENS`
(verified in Claude Code 2.1.205): the window function uses this value for any model id that does
**not** start with `claude-`, which is exactly the mapped-model case:

```bash
# e.g. gpt-5.6-sol's real window
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000
```

Caveats: it is **global** — one value for every non-`claude-` model in the session, so it can't be
set per-model when routing models with different window sizes — and setting it **larger than the
real upstream window delays auto-compact past the point where the upstream rejects the request**
with a context-length error, so match it to the smallest real window among your mapped models.
Claude passthrough models (`claude-*` ids) ignore it and keep their exact built-in sizes. (With
`DISABLE_COMPACT` also set, the value applies unconditionally — `claude-*` ids included.)

The other client-side lever is the `[1m]` model-id suffix, which forces a **1M** window — useful
for a genuinely 1M-context model, but misleading (under-reporting) for a smaller one, so avoid it
unless the upstream really has that window.

| Field | Mapped (`responses`) model | Claude passthrough |
| :-- | :-- | :-- |
| Context tokens used | ✅ accurate (forwarded by shunt) | ✅ accurate |
| Context window (denominator) | ⚠️ 200k default; set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` (or `[1m]` → 1M) | ✅ exact |
| `count_tokens` (pre-flight) | ⚠️ client `char/4`, or `count_tokens = "tiktoken"` for a closer local count (§4) | ✅ exact (upstream) |
| `rate_limits` (5h / weekly) | ❌ needs Anthropic `anthropic-ratelimit-*` headers | ✅ shown |

### 5.9 (Optional) Shared-gateway client tokens

By default shunt has no inbound auth — fine for a loopback-only personal gateway, but once
you share it over a VPN/tunnel, anyone who can reach it can spend the **operator's** account
on mapped models (shunt injects its own `api_key`/`chatgpt_oauth` credential for those).
Passthrough models are not the concern: they forward each caller's own Anthropic credential.

`[server.auth]` gates exactly the injected-credential routes with per-client tokens
(spec: [`m4-inbound-auth.md`](m4-inbound-auth.md)):

```toml
[server.auth]                        # both keys optional; defaults shown
header = "x-shunt-token"
tokens_env = "SHUNT_CLIENT_TOKENS"
```

```bash
# Gateway side: name:token pairs (names are labels for logging; tokens are secrets)
export SHUNT_CLIENT_TOKENS="minsu:$(openssl rand -hex 32),alice:$(openssl rand -hex 32)"
```

Startup **fails closed** if `[server.auth]` is present but the env var is unset or
malformed. Requests to mapped models without a valid token get a 401
`authentication_error`; `GET /v1/models`, `GET|HEAD /`, `GET /health`, and passthrough
models stay open.
The token header is always stripped before forwarding, matching is constant-time, and
token values are never logged (client *names* are, per request).

Client side, one line (`ANTHROPIC_CUSTOM_HEADERS` takes one `Name: Value` per line):

```bash
export ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <your token>"
```

This is application-layer identification only — transport encryption still comes from the
deployment (WireGuard/Tailscale tunnel, or TLS termination in front); shunt itself serves
plain HTTP.

### 5.10 SSE keepalive pings (Cloudflare 524 survival)

Middleboxes kill quiet streams — Cloudflare's proxy returns **524 after 100 seconds without
a byte** (fixed below Enterprise), and long reasoning stretches can be silent that long.
shunt therefore injects the Anthropic protocol's own `ping` event (which api.anthropic.com
itself emits and every client ignores) whenever a streaming response has been idle:

```toml
[server]
sse_keepalive_seconds = 30   # default; 0 disables
```

Pings are injected only between complete SSE events (never inside a half-sent frame), only
on `text/event-stream` responses, and stop with the upstream stream. Behind a tunnel with
no idle timeout (WireGuard/Tailscale) the pings are harmless; disable with `0` if you want
byte-identical relaying. Spec: [`m5-sse-keepalive.md`](m5-sse-keepalive.md).

---

## 6. Verify

**1. Test the gateway directly** (proves the URL + routing work before opening Claude Code):

```bash
# Unmapped model -> forwarded to Anthropic (uses your Anthropic credential)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-4-6","max_tokens":1,"messages":[{"role":"user","content":"."}]}'

# Mapped model -> diverted to the provider (uses shunt's provider credential)
curl -s -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"gpt-5.6-sol","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
```

A JSON response starting with `{"id":"msg_` means it worked.

**2. Confirm inside Claude Code:** start `claude` from the same shell, run `/status`, and check
the **Anthropic base URL** line shows `http://127.0.0.1:3001`. Then open `/model` and pick your
mapped model.

---

## 7. Troubleshooting

| Symptom | Cause / Fix |
| :------ | :---------- |
| `ChatGPT auth not found; run codex login` | shunt can't read `~/.codex/auth.json`. Run `codex login`. |
| `authentication_error` on a mapped model | Expired/absent provider credential — re-run `codex login`, or export `OPENAI_API_KEY`. shunt surfaces the backend's real `detail` message. |
| `400 ... model is not supported when using Codex with a ChatGPT account` | You used a `-codex` slug (or one your account isn't entitled to). Use an entitled slug from [models.json](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) (e.g. `gpt-5.6-sol`, `gpt-5.5`) or set `upstream_model`. |
| `/model` doesn't list your model | For `gpt-*` ids use `ANTHROPIC_CUSTOM_MODEL_OPTION`; discovery only surfaces `claude`/`anthropic`-prefixed ids. |
| `config check failed` | Run `shunt check` for the exact reason (bind address, unknown provider in a route, wrong adapter/auth). |
| Claude Code asks you to log in | Set an Anthropic credential (`ANTHROPIC_AUTH_TOKEN` / login) that shunt can forward for unmapped models. A base URL alone is not a credential. |

For the full gateway troubleshooting table, see
[Connect Claude Code to an LLM gateway](https://code.claude.com/docs/en/llm-gateway-connect#troubleshoot-gateway-errors).

---

## 8. Quick start (copy-paste)

```bash
# 1. Build
cargo build --release

# 2. Configure
cp shunt.toml.example shunt.toml
./target/release/shunt check

# 3. Provider credential (pick one)
codex login                    # Codex/ChatGPT provider
# export OPENAI_API_KEY=sk-... # OpenAI provider

# 4. Run the gateway
./target/release/shunt run &

# 5. Point Claude Code at it and select a mapped model
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1   # so /effort maps to reasoning.effort (§5.6)
claude                         # then /model -> pick gpt-5.6-sol
```
