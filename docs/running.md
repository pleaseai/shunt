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
2. A **TOML file**, `./shunt.toml` by default (override with `--config <path>`).
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

[providers.anthropic]
base_url = "https://api.anthropic.com"

[providers.openai]
adapter = "responses"          # must be "responses"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY" # env var the OpenAI key is read from
# effort = "high"              # optional default reasoning effort for this provider

[providers.codex]
adapter = "responses"          # must be "responses"
base_url = "https://chatgpt.com/backend-api"
# auth = "chatgpt_oauth"       # (default) reuses ~/.codex/auth.json
# effort = "high"

# --- Routing: how a request's `model` id picks a provider ---

# Exact match wins first. `upstream_model` and `effort` are optional overrides.
[[routes]]
model = "gpt-5.5"
provider = "codex"
# upstream_model = "gpt-5.5"
# effort = "high"

# Then prefix match.
[[route_prefixes]]
prefix = "gpt-"
provider = "openai"

# Optional: expose Claude-named aliases in the /model picker via discovery (§5.4).
# The id MUST start with "claude" or "anthropic" or Claude Code ignores it.
# [[models]]
# id = "claude-opus-via-codex"
# display_name = "Opus (via Codex)"
```

**Routing precedence** (`src/routing.rs`): exact `[[routes]]` match → `[[route_prefixes]]`
prefix match → `server.default_provider`. A model with no match falls through to Anthropic.

### 3.2 Validate the config

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
| `GET`  | `/v1/models`                  | Model discovery (returns your `[[models]]` entries) |
| `POST` | `/v1/messages`                | Inference — routed per the request's `model` id     |
| `POST` | `/v1/messages/count_tokens`   | Token counting (passed through for Anthropic models)|

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
`ANTHROPIC_AUTH_TOKEN`). shunt **forwards it unchanged** to `api.anthropic.com` for every model
you didn't map, so unmapped models keep working exactly as before. Provider credentials for
mapped models are injected by shunt itself (§5.3) — Claude Code never sends them.

### 5.2 Provide the mapped provider's credential (to shunt, not Claude Code)

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

### 5.3 Select a mapped model (primary path)

Claude Code's model-discovery only honors ids beginning with `claude`/`anthropic`, so for
OpenAI/Codex ids (`gpt-*`) use `ANTHROPIC_CUSTOM_MODEL_OPTION` — it adds a picker entry whose id
skips validation:

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.5"
```

Then pick it from `/model` in Claude Code. That id is what shunt routes on, so it must match a
`[[routes]]`/`[[route_prefixes]]` rule in your config.

> **Model slugs (verified 2026-07-09):** the ChatGPT-account Codex backend **rejects**
> `gpt-*-codex` slugs (e.g. `gpt-5.2-codex`) — it only accepts the account's live-entitled
> slugs. A **free** ChatGPT account resolves to **`gpt-5.5`**. Use `upstream_model` in a route,
> or pass an entitled slug via `ANTHROPIC_CUSTOM_MODEL_OPTION`. See
> [`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md) §0.

Per-context selection also works via Claude Code's own knobs — a subagent's `model:`
frontmatter, or `CLAUDE_CODE_SUBAGENT_MODEL` for all subagents — so you can divert only one
agent while the main session stays on Claude.

### 5.4 (Optional) Model discovery

Discovery (`GET /v1/models`) can populate `/model` automatically — **but Claude Code ignores
any id that doesn't begin with `claude`/`anthropic`** ([protocol
reference](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery)). So a `gpt-*`
id is dropped client-side no matter what; discovery is only useful when you expose a
**Claude-named alias** that a `[[routes]]` entry rewrites to the real upstream slug:

```toml
[[models]]
id = "claude-gpt-5.5-via-codex"        # must begin with claude/anthropic
display_name = "GPT-5.5 (via Codex)"

[[routes]]
model = "claude-gpt-5.5-via-codex"     # the alias Claude Code sends
provider = "codex"
upstream_model = "gpt-5.5"             # real slug forwarded to the ChatGPT backend
```

Then enable discovery (Claude Code v2.1.129+) and restart shunt + Claude Code:

```bash
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
```

The alias appears in `/model` labeled *From gateway*; selecting it sends
`claude-gpt-5.5-via-codex`, which shunt routes to `codex` and rewrites to `gpt-5.5`. Discovery
fails **silently** (3-second timeout, any redirect counts as failure) and falls back to the
cached/built-in list — run `claude --debug` and look for `[gatewayDiscovery]` lines to confirm
it ran. For `gpt-*` ids without an alias, use `ANTHROPIC_CUSTOM_MODEL_OPTION` (§5.3) instead.
See [`m3-discovery.md`](m3-discovery.md).

> **Discovery needs a gateway credential — a claude.ai OAuth *login* alone won't trigger it.**
> Claude Code only issues the `/v1/models` request when `ANTHROPIC_AUTH_TOKEN` (or an API key)
> is set; under a plain Max/Pro subscription login it sends nothing (no request reaches shunt,
> no cache is written) even with the flag on. Verified: shunt served `/v1/models` correctly, but
> the request never left Claude Code until a token was set.
>
> Use a **Claude Code OAuth token** (`claude setup-token`, `sk-ant-oat…`) as
> `ANTHROPIC_AUTH_TOKEN` — one value covers both roles:
>
> ```bash
> export ANTHROPIC_AUTH_TOKEN=sk-ant-oat…   # from `claude setup-token`
> export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1
> ```
>
> | Credential | Discovery label | Claude passthrough models | Billing |
> | :-- | :-- | :-- | :-- |
> | OAuth **login** only (no token) | ❌ never fires | ✅ | subscription |
> | `ANTHROPIC_AUTH_TOKEN=sk-ant-oat…` (setup-token) | ✅ | ✅ | subscription |
> | `ANTHROPIC_AUTH_TOKEN=sk-dummy` | ✅ | ❌ 401 (dummy forwarded to Anthropic) | — |
> | `ANTHROPIC_AUTH_TOKEN=<real API key>` | ✅ | ✅ | **API (not subscription)** |
>
> The `setup-token` OAuth token is the best of both: it satisfies the discovery gate *and*
> authenticates Claude-passthrough models on your subscription. `gpt-*` models are unaffected
> either way — shunt injects the ChatGPT/OpenAI credential for them regardless of this token.
>
> **Prefer `claude setup-token`.** It mints a **one-year** OAuth token
> ([authentication docs](https://code.claude.com/docs/en/authentication#generate-a-long-lived-token)),
> so nothing needs refreshing. The short-lived access token inside `~/.claude/.credentials.json`
> (macOS: Keychain) expires in hours and, once a gateway credential is active, Claude Code stops
> refreshing the login — so a helper that just reads that file breaks after expiry. If you want to
> reuse the live subscription login instead of minting a separate token, use the built-in
> `shunt token` helper (below), which refreshes it for you. Refreshing manually is otherwise
> discouraged: `platform.claude.com/v1/oauth/token` is aggressively rate-limited/WAF-guarded (a
> single stray call can return `429`), and it rewrites your live login file.

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

### 5.5 Reasoning effort

Claude Code's effort level (`/effort`, the `/model` slider, `--effort`, or
`CLAUDE_CODE_EFFORT_LEVEL`) is sent as the `output_config.effort` request field, and shunt maps
it to the Responses `reasoning.effort` for mapped models:

| Claude Code effort | → `reasoning.effort` |
| :-- | :-- |
| `low` / `medium` / `high` / `xhigh` | passthrough |
| `max` | `xhigh` (the Responses API has no `max`) |

**For a custom gateway id like `gpt-5.5` you must set `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`** —
otherwise Claude Code omits `output_config.effort` for model ids it doesn't recognize as
effort-capable, and shunt falls back to `medium`.

```bash
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
```

Precedence in shunt: a config `route.effort` / `[providers.*].effort` override wins first;
otherwise the request's `output_config.effort` is honored; otherwise `thinking.enabled → high`,
then a model-name suffix (`-xhigh`/`-high`/`-medium`/`-low`), else `medium`.

### 5.6 (Optional) Attribution block

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
  -d '{"model":"gpt-5.5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'
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
| `400 ... model is not supported when using Codex with a ChatGPT account` | You used a `-codex` slug. Use an entitled slug (e.g. `gpt-5.5`) or set `upstream_model`. |
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
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.5"
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1   # so /effort maps to reasoning.effort (§5.5)
claude                         # then /model -> pick gpt-5.5
```
