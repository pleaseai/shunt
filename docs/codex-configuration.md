# Codex / ChatGPT provider — configuration reference

How to route Claude Code inference to your **ChatGPT / Codex subscription** through shunt,
reusing the credential the Codex CLI already wrote to `~/.codex/auth.json`. No API key, no
per-token billing — the request is authenticated as your ChatGPT account and answered by the
same backend the `codex` CLI talks to.

This page consolidates every Codex-specific knob. For the broader gateway workflow see
[`running.md`](running.md); for the credential-acquisition spec see
[`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md).

- [1. How it fits together](#1-how-it-fits-together)
- [2. Prerequisites](#2-prerequisites)
- [3. The `[providers.codex]` block](#3-the-providerscodex-block)
- [4. Authentication (`~/.codex/auth.json`)](#4-authentication-codexauthjson)
- [5. Model slugs](#5-model-slugs)
- [6. Routing a model to Codex](#6-routing-a-model-to-codex)
- [7. Selecting the model in Claude Code](#7-selecting-the-model-in-claude-code)
- [8. Reasoning effort](#8-reasoning-effort)
- [9. Context window & usage display](#9-context-window--usage-display)
- [10. `count_tokens` behavior](#10-count_tokens-behavior)
- [11. Attribution header](#11-attribution-header)
- [12. Security](#12-security)
- [13. Troubleshooting](#13-troubleshooting)
- [14. End-to-end example](#14-end-to-end-example)

---

## 1. How it fits together

The `codex` provider is a built-in **`kind = "responses"`** provider: shunt translates Claude
Code's Anthropic Messages request into the OpenAI **Responses API** shape, then sends it to the
ChatGPT-account Codex backend and translates the streamed response back. Three things make it
"Codex" rather than plain OpenAI:

| Aspect | Value | Source |
| :-- | :-- | :-- |
| Upstream endpoint | `<base_url>/codex/responses` | `src/adapters/responses.rs` |
| Auth | ChatGPT OAuth from `~/.codex/auth.json` (auto-refreshed) | `src/auth/codex_auth.rs` |
| Responses dialect | `ResponsesFlavor::Chatgpt` — drops params codex never sends | `src/config.rs`, `src/model/responses_request.rs` |

The `Chatgpt` flavor is detected from `auth = "chatgpt_oauth"` (not the provider name), so the
per-backend quirks apply to any provider that uses that auth mode. Notably the ChatGPT backend
**rejects `max_output_tokens`** (`"Unsupported parameter: 'max_output_tokens'"`), so shunt drops
it for this flavor only; it also sends `store: false` and round-trips the encrypted reasoning
blob so chain-of-thought survives across turns.

---

## 2. Prerequisites

Log in once with the Codex CLI. shunt reads and refreshes the file it writes — it does **not**
initiate its own login for Codex:

```bash
codex login
```

This creates `~/.codex/auth.json`. If that file is missing, has no tokens, or the refresh token
is gone, shunt returns an `authentication_error` whose message tells you to run `codex login`
again.

---

## 3. The `[providers.codex]` block

The `codex` provider is **built in** — you do not need to declare it at all. This is the full
default; every key shown is the value shunt uses when you omit it:

```toml
[providers.codex]
kind = "responses"                       # translate Anthropic Messages <-> OpenAI Responses
base_url = "https://chatgpt.com/backend-api"  # shunt appends /codex/responses
auth = "chatgpt_oauth"                   # read + auto-refresh ~/.codex/auth.json
# api_key_header = "bearer"              # unused for chatgpt_oauth (bearer is implicit)
# effort = "high"                        # optional default reasoning effort (see §8)
# count_tokens = "tiktoken"              # default; "estimate" opts out (see §10)
```

A partial `[providers.codex]` table overrides **only** the keys it sets — the built-in defaults
fill the rest. Practical uses:

- **Pin a default effort** for everything routed to Codex: `effort = "high"`.
- **Opt out of local token counting**: `count_tokens = "estimate"` (see §10).
- **Point at a different backend host** (rare): change `base_url`. shunt still appends
  `/codex/responses` and still sends the ChatGPT OAuth headers, so the host must be the ChatGPT
  Codex backend.

Keys that do **not** apply to `chatgpt_oauth`: `api_key_env` and `api_key_header` are for
`auth = "api_key"` providers only. The credential comes from the auth file, not the environment.

> **Two accounts, one CLI:** if `~/.codex/auth.json` is in **`ApiKey`** mode (you logged in with
> an OpenAI API key rather than a ChatGPT account), the `codex` provider's OAuth path finds no
> tokens and errors. That API key is instead picked up by the **`openai`** provider as a fallback
> when `OPENAI_API_KEY` is unset (`src/auth/mod.rs`). The `codex` provider is specifically the
> ChatGPT-subscription path.

---

## 4. Authentication (`~/.codex/auth.json`)

### 4.1 File location

shunt resolves the auth-file path in this order (`default_codex_auth_path`, `src/auth/mod.rs`):

1. `$CODEX_AUTH_FILE` if set — point shunt at a non-standard location (CI, a sandbox, a second
   account).
2. `$HOME/.codex/auth.json`.
3. `.codex/auth.json` relative to the working directory (last-resort fallback).

```bash
# Example: run shunt against an auth file outside the home directory
export CODEX_AUTH_FILE=/etc/shunt/codex-auth.json
```

### 4.2 File schema

shunt reads (and, on refresh, rewrites) this JSON, written by `codex login`:

```jsonc
{
  "auth_mode": "ChatGPT",          // "ApiKey" routes to the openai provider instead
  "OPENAI_API_KEY": null,          // a string only in ApiKey mode
  "tokens": {
    "id_token":      "<JWT>",
    "access_token":  "<JWT>",      // bearer sent upstream; carries exp + account claim
    "refresh_token": "<JWT>",      // used to mint a new access token
    "account_id":    "<uuid>"      // preferred account-id source
  },
  "last_refresh": "2026-07-11T09:00:00Z"
}
```

- **Account id** — shunt prefers `tokens.account_id`; if absent it decodes the `access_token`
  JWT payload and reads `["https://api.openai.com/auth"].chatgpt_account_id`. If neither exists
  the request fails with `ChatGPT account id missing; run codex login`.
- **Expiry** — there is no `expires_at` field. shunt reads the `exp` claim from the
  `access_token` JWT and treats the token as expired **5 minutes early** (`EXPIRY_BUFFER`), so a
  request in flight never races the expiry.

### 4.3 Read / refresh / write-back cycle

On every routed request (`get_valid_chatgpt`, `src/auth/codex_auth.rs`):

1. **Read fresh** from the auth file — the Codex CLI may have refreshed it under shunt, so
   re-reading avoids a redundant refresh and a clobber.
2. If the access token is valid (`now < exp − 5min`), use it as-is.
3. Otherwise **re-read once more** (a concurrent `codex` process may have just refreshed it), and
   only if still expired, refresh:
   - `POST https://auth.openai.com/oauth/token`, form-encoded:
     `grant_type=refresh_token`, `refresh_token=<current>`,
     `client_id=app_EMoamEEZ73f0CkXaXp7hrann`.
   - The response yields a new `access_token`, and possibly a rotated `refresh_token` and
     `id_token`.
4. **Write back atomically**: re-read the file, update `tokens.{access_token, refresh_token,
   id_token}`, recompute `account_id` from the new access token, set `last_refresh`, and
   **preserve every other field** (`auth_mode`, `OPENAI_API_KEY`, and any unknown keys the Codex
   CLI owns). The write goes to a private temp file (`0600`, created exclusively) and is renamed
   into place, so tokens are never briefly world-readable.

You don't configure any of this — it is automatic. The only knob is the file path (§4.1).

### 4.4 Headers shunt sends upstream

For a Codex request shunt sends the Codex-CLI identity so client-version gating (§5) passes
(`src/adapters/responses.rs`):

| Header | Value |
| :-- | :-- |
| `authorization` | `Bearer <access_token>` |
| `chatgpt-account-id` | `<account_id>` |
| `originator` | `codex_cli_rs` |
| `user-agent` | `codex_cli_rs/0.144.1` (`CODEX_USER_AGENT`) |
| `version` | `0.144.1` (`CODEX_CLIENT_VERSION`) |
| `OpenAI-Beta` | `responses=experimental` |
| `content-type` | `application/json` |

The `user-agent` / `version` are **pinned to openai/codex rust-v0.144.1**. If a future slug
demands a newer client, bump `CODEX_USER_AGENT` / `CODEX_CLIENT_VERSION` in
`src/adapters/responses.rs`.

---

## 5. Model slugs

The ChatGPT-account Codex backend only accepts the **slugs your account is currently entitled
to**, and **rejects the `gpt-*-codex` slugs** (e.g. `gpt-5.2-codex`) with a `400`
`"The 'X' model is not supported when using Codex with a ChatGPT account."` Do **not** invent a
`-codex` model id.

- The authoritative catalog of Codex slugs (and the reasoning levels each accepts) is openai/codex's
  [`codex-rs/models-manager/models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json).
- Current listed slugs: **`gpt-5.6-sol`**, **`gpt-5.6-terra`**, **`gpt-5.6-luna`** (latest,
  frontier), and **`gpt-5.5`** / **`gpt-5.4`** / **`gpt-5.4-mini`** / **`gpt-5.2`**. Older
  accounts may only be entitled to the earlier ones; a **free** account has resolved to `gpt-5.5`
  in testing.
- To see what your account can use, look at what the `codex` CLI itself sends, or the live
  `/models` fetch it performs at startup.

> **Client-version gating.** Some slugs carry a `minimal_client_version` (e.g. `gpt-5.6-luna`
> needs ≥ 0.144.0). When the request's client identity is missing or too old the backend answers
> **`Model not found <slug>`** — *not* an entitlement error. shunt avoids this by sending the
> pinned Codex CLI headers (§4.4). See [openai/codex#31967](https://github.com/openai/codex/issues/31967).

shunt surfaces the backend's own `detail` message on error, so a wrong or unentitled slug returns
the real reason rather than a generic failure.

---

## 6. Routing a model to Codex

A request's `model` id selects the provider. Precedence: exact `[[routes]]` → `[[route_prefixes]]`
→ `server.default_provider`.

### 6.1 Exact route

```toml
[[routes]]
model = "gpt-5.6-sol"     # the id Claude Code sends (see §7)
provider = "codex"
# upstream_model = "gpt-5.6-sol"   # optional: rewrite to a different slug upstream
# effort = "high"                  # optional: pin effort for this route (see §8)
```

`upstream_model` lets the id Claude Code sends differ from the slug the backend receives — the
mechanism behind discovery aliases (§7) and a way to swap the real slug without touching your
Claude Code env.

### 6.2 Prefix route

Send every `gpt-*` id to Codex with one rule (note: the built-in example config points `gpt-` at
the `openai` provider — change it to `codex` if you want the subscription path):

```toml
[[route_prefixes]]
prefix = "gpt-5.6-"
provider = "codex"
```

---

## 7. Selecting the model in Claude Code

Claude Code's `/model` picker only honors discovery ids that begin with `claude`/`anthropic`, so
a raw `gpt-*` id needs one of two paths (or remap the tier aliases entirely — §7.4). **They don't
overlap** — the split is on the `claude-`/`anthropic-` prefix:

| What | `claude-…` discovery alias | non-`claude-` id (e.g. `gpt-5.6-sol`) |
| :-- | :-- | :-- |
| `/v1/models` discovery → `/model` picker | ✅ auto-listed ("From gateway"), many models | ❌ dropped by Claude Code |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | ❌ not honored | ✅ adds to picker (**one id only**) |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` window (§9) | ❌ ignored → 200k default | ✅ applies → real window |

### 7.1 Primary path — `ANTHROPIC_CUSTOM_MODEL_OPTION`

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
```

Adds a picker entry whose id skips validation; that id is exactly what shunt routes on, so it
must match a `[[routes]]`/`[[route_prefixes]]` rule. This is the recommended path — it's the only
one that also lets you set an accurate context window (§9).

### 7.2 Discovery alias — a `claude-`-named alias rewritten to a Codex slug

Convenient when you want several Codex models auto-listed in the picker. Expose a `claude-`-named
alias and rewrite it upstream:

```toml
[[models]]
id = "claude-gpt-5.6-sol-via-codex"     # MUST begin with claude/anthropic
display_name = "GPT-5.6-Sol (via Codex)"

[[routes]]
model = "claude-gpt-5.6-sol-via-codex"  # the alias Claude Code sends
provider = "codex"
upstream_model = "gpt-5.6-sol"          # real slug forwarded to the ChatGPT backend
```

```bash
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1   # Claude Code v2.1.129+
```

The alias shows in `/model` as *From gateway*. **Trade-off:** because the alias begins with
`claude-`, `CLAUDE_CODE_MAX_CONTEXT_TOKENS` can't reach it, so its context bar stays pinned at the
200k default (safe, but under-sized). Use `ANTHROPIC_CUSTOM_MODEL_OPTION` when you need the real
window. Discovery also needs a gateway credential set (an API key / `ANTHROPIC_AUTH_TOKEN` /
`apiKeyHelper`) — a plain Max/Pro login alone won't trigger the `/v1/models` request. See
[`running.md`](running.md) §5.5 and [`m3-discovery.md`](m3-discovery.md).

### 7.3 Subagents

You can run a subagent on a Codex slug while the main session stays on Claude. The `model:`
frontmatter field is the key: it accepts **any string** (unlike the Agent/Task tool's `model`
parameter, which only takes the built-in aliases `opus`/`sonnet`/`haiku`/`fable`).

**Point an existing subagent at a Codex slug** — edit its `.claude/agents/<name>.md` frontmatter
and set (or add) `model:`. For example, to move an existing `researcher` agent onto `gpt-5.6-sol`:

```markdown
---
name: researcher
description: Deep research agent.
model: gpt-5.6-sol        # was: sonnet (or absent → inherited)
---

<the agent's system prompt — unchanged>
```

Then spawn it by its type **without** a `model` override — the tool parameter outranks frontmatter,
so passing one would shadow the slug. Resolution order:
`CLAUDE_CODE_SUBAGENT_MODEL` > Agent/Task tool `model` > frontmatter `model:` > `inherit`.

**Force every subagent onto one Codex slug** — set the env var (highest precedence, global):

```bash
export CLAUDE_CODE_SUBAGENT_MODEL="gpt-5.6-sol"
```

Either way the id must have a `[[routes]]` entry (§6) and, being non-`claude-`, obeys
`CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` (§8) and `CLAUDE_CODE_MAX_CONTEXT_TOKENS` (§9) — the context
window follows the id automatically, so one global value sizes the mapped subagent while the Claude
main keeps its own.

The **[`shunt-codex` plugin](../plugins/shunt-codex/)** ships ready-made subagents for
`gpt-5.6-sol` / `-terra` / `-luna` (each pins its `model:` frontmatter to the slug), so you can
`@`-mention a Codex model without authoring the agent files yourself.

### 7.4 Remap the tier aliases (`haiku`/`sonnet`/`opus` → Codex)

Instead of adding one custom id, you can repoint Claude Code's **built-in tier aliases** at Codex
slugs, so the whole session's tier system resolves to your ChatGPT subscription
([model-config env vars](https://code.claude.com/docs/en/model-config#environment-variables)):

| Env var | Controls |
| :-- | :-- |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | what the `haiku` alias **and the background/"small-fast" model** resolve to |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | what the `sonnet` alias resolves to |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` | what the `opus` alias resolves to |
| `ANTHROPIC_DEFAULT_FABLE_MODEL` | what the `fable` alias resolves to |

**Example — the two-tier setup** (`haiku → gpt-5.6-luna`, `sonnet → gpt-5.6-sol`):

```bash
export ANTHROPIC_DEFAULT_HAIKU_MODEL="gpt-5.6-luna"
export ANTHROPIC_DEFAULT_SONNET_MODEL="gpt-5.6-sol"
```

```toml
# shunt.toml — both resolved ids must have a route
[[routes]]
model = "gpt-5.6-luna"
provider = "codex"

[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
```

Now selecting **Sonnet** in `/model` runs `gpt-5.6-sol` via Codex, and every background/haiku task
runs `gpt-5.6-luna`. The resolved id (`gpt-5.6-sol` / `gpt-5.6-luna`) is exactly what shunt routes
on — no `ANTHROPIC_CUSTOM_MODEL_OPTION` needed.

**Nicer picker labels** — the `_NAME` / `_DESCRIPTION` companions take effect on a gateway
(`ANTHROPIC_BASE_URL` → shunt), so the raw slug isn't shown as-is:

```bash
export ANTHROPIC_DEFAULT_SONNET_MODEL_NAME="GPT-5.6-Sol"
export ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION="ChatGPT/Codex Sol via shunt"
export ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME="GPT-5.6-Luna"
export ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION="ChatGPT/Codex Luna via shunt (background tier)"
```

Things to get right:

- **These ids don't start with `claude-`**, so `CLAUDE_CODE_MAX_CONTEXT_TOKENS` (§9) applies and
  `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` is needed for effort (§8). Handily, `gpt-5.6-sol` and
  `gpt-5.6-luna` are **both 372k**, so one global `CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000` fits both
  tiers.
- **The `_SUPPORTED_CAPABILITIES` companion is documented for third-party providers (Bedrock, etc.),
  not confirmed for gateways** — on shunt, stick with `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` to enable
  the effort dial.
- **The haiku tier is Claude Code's background "small-fast" model** (`ANTHROPIC_SMALL_FAST_MODEL`,
  now deprecated in favor of this alias) — used for cheap, frequent work like conversation
  summaries, titles, and quick classification. Routing it to a full reasoning model like Luna is
  fine, but it spends your ChatGPT quota on that background traffic and can be slower; pick your
  cheapest entitled slug for the haiku tier if that matters.
- **This is global and session-wide.** If an allowlist is in force (`availableModels` /
  `enforceAvailableModels`), an alias can't be redirected to a model outside the list (Claude Code
  enforces this on the tier-alias env vars as of **v2.1.176**).

---

## 8. Reasoning effort

Claude Code's effort level (`/effort`, the `/model` slider, `--effort`, or
`CLAUDE_CODE_EFFORT_LEVEL`) is sent as `output_config.effort`, which shunt maps to the Responses
`reasoning.effort`:

| Claude Code effort | → `reasoning.effort` |
| :-- | :-- |
| `low` / `medium` / `high` / `xhigh` | passthrough |
| `max` | passthrough on slugs that accept it (the **gpt-5.6** family), else folded to `xhigh` |

`max` folds to `xhigh` unless the upstream slug contains `gpt-5.6` (`supports_max_effort`,
`src/model/responses_request.rs`), because `models.json` `supported_reasoning_levels` caps
`gpt-5.5`/`5.4`/`5.2` at `xhigh` while the gpt-5.6 family accepts `max`.

> **Required for custom gateway ids.** For an id Claude Code doesn't recognize as effort-capable
> (like `gpt-5.6-sol`), Claude Code **omits** `output_config.effort` unless you set:
>
> ```bash
> export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
> ```
>
> Without it, shunt never sees a client effort and falls back to `medium`.

**Precedence in shunt** (`effort()`, `src/model/responses_request.rs`):

1. A config `route.effort` — or `[providers.codex].effort` — override wins.
2. Else the request's `output_config.effort`.
3. Else `thinking.enabled` → `high`.
4. Else a model-name suffix: `-xhigh` / `-high` / `-medium` / `-low` (with `-spark` treated as
   `-low`).
5. Else `medium`.

So `effort = "high"` under `[providers.codex]` pins high effort for all Codex traffic regardless
of the Claude Code slider; drop it to let the client control effort per-turn.

---

## 9. Context window & usage display

Claude Code computes the context indicator **locally**: `usage` tokens ÷ the model's window size.

- **Numerator is accurate.** shunt forwards the Responses `usage` (`input_tokens`, peeling the
  cached part into `cache_read_input_tokens`), so the bar fills correctly as the conversation
  grows.
- **Denominator defaults to 200k for mapped ids.** Claude Code's `getContextWindowForModel`
  returns `200_000` for any id it doesn't recognize (its accurate per-model lookup only runs when
  the base URL is `api.anthropic.com`). A larger real window (e.g. `gpt-5.6-sol` at 372k) shows a
  conservative, over-reported percentage — harmless except that auto-compact triggers early.

Override the denominator for non-`claude-` ids (verified in Claude Code 2.1.205):

```bash
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000   # gpt-5.6-sol's real window
```

Caveats:

- **Global** — one value for every non-`claude-` model in the session. Match it to the
  **smallest** real window among your mapped models.
- **Don't set it larger than the real window.** On the streaming path Claude Code uses, an
  overflow surfaces mid-stream as `prompt is too long`; shunt normalizes it
  (`context_overflow_message`, `src/model/responses.rs`) and Claude Code auto-compacts and
  retries — the session recovers, but every overflow round-trip is wasted latency. (Live-verified
  for `gpt-5.6-sol`: 365k answers normally, 372k+ overflows — the boundary is its `models.json`
  `context_window` of 372000; `gpt-5.5` is 272000.)
- A `claude-…-via-codex` **discovery alias ignores this override** (the `claude-` gate) — its
  window stays at 200k. Use a non-`claude-` id for the accurate window.

The `[1m]` id suffix forces a 1M window client-side; shunt strips a trailing `[1m]` before route
matching and before forwarding upstream, so `gpt-5.6-sol[1m]` still routes correctly. Only use it
if the upstream genuinely has a 1M window, or it under-reports.

| Field | Codex (`responses`) model | Claude passthrough |
| :-- | :-- | :-- |
| Context tokens used | ✅ accurate (forwarded by shunt) | ✅ accurate |
| Context window (denominator) | ⚠️ 200k default; set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` | ✅ exact |
| `count_tokens` (pre-flight) | ⚠️ local tiktoken or client `char/4` (§10) | ✅ exact (upstream) |
| `rate_limits` (5h / weekly) | ❌ needs Anthropic headers | ✅ shown |

---

## 10. `count_tokens` behavior

The Responses API has no server-side token-count endpoint, so shunt answers Claude Code's
pre-flight `POST /v1/messages/count_tokens` itself. Controlled by `count_tokens` under
`[providers.codex]`:

| Value | Behavior |
| :-- | :-- |
| `"tiktoken"` (**default**) | Count locally with the `o200k_base` encoder (the GPT-family tokenizer) and return `{"input_tokens": N}`. Near-exact for text; it can't see the backend's image/tool-schema encoding, reasoning tokens, or cache accounting. |
| `"estimate"` | Return `404` so Claude Code falls back to its own estimate. Its `/context` then re-counts every category against Haiku over the network — slow, and silently zero without an Anthropic credential — so this is opt-in. |

```toml
[providers.codex]
count_tokens = "estimate"   # opt out of local tiktoken counting
```

The default (`tiktoken`) is the better choice for most Codex users — it's far closer than Claude
Code's `char/4` fallback and needs no network round-trip. See `src/count_tokens.rs`.

---

## 11. Attribution header

Claude Code prepends an attribution line to the system prompt
(`x-anthropic-billing-header: cc_version=…`). Anthropic strips it; a Codex backend receives it as
the first line of `instructions`. It's harmless but meaningless. To drop it:

```bash
export CLAUDE_CODE_ATTRIBUTION_HEADER=0
```

Global — it also removes attribution from any Anthropic-passthrough traffic (used for cost
tracking), which is fine when routing to Codex.

---

## 12. Security

- **Tokens are never logged.** shunt logs only non-secret facts (auth mode, account-id presence,
  expiry, refresh success/failure).
- **File permissions.** On Unix/Linux/macOS any auth file shunt writes is created `0600` via an
  exclusive temp file + atomic rename — tokens are never momentarily world-readable. On non-Unix
  platforms the temp file is written with `fs::write` (no `0600` or exclusive-create guarantee).
- **Treat `~/.codex/auth.json` as sensitive.** It is **not** in `.worktreeinclude`, so it isn't
  copied into Orca worktrees; don't copy it into logs, telemetry, or shared checkouts.
- **The refresh endpoint is OpenAI's own** (`auth.openai.com/oauth/token`) with the public Codex
  CLI `client_id`; shunt sends the refresh token there and nowhere else.

---

## 13. Troubleshooting

| Symptom | Likely cause / fix |
| :-- | :-- |
| `authentication_error: ChatGPT auth not found; run codex login` | No `~/.codex/auth.json` (or wrong `$CODEX_AUTH_FILE`). Run `codex login`. |
| `ChatGPT auth tokens missing` / `refresh token missing` | Auth file is in `ApiKey` mode or truncated — that path is the `openai` provider, not `codex`. Re-`codex login` with a ChatGPT account. |
| `400 … not supported when using Codex with a ChatGPT account` | You used a `gpt-*-codex` slug. Use an entitled non-`-codex` slug (§5). |
| `Model not found <slug>` | Client-version gating or an unentitled slug — not a code error. Confirm the slug via `models.json`; shunt already sends the pinned CLI headers (§4.4). |
| Effort slider seems ignored on a `gpt-*` id | Set `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` (§8), or a `route`/`provider` `effort` override is winning. |
| Context bar over-reports / compacts early | Set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` to the real window (§9). A discovery alias can't take it — use a non-`claude-` id. |
| `prompt is too long` churn mid-session | `CLAUDE_CODE_MAX_CONTEXT_TOKENS` is set larger than the real window. Lower it to the smallest mapped window. |
| `gpt-*` model never appears in `/model` | Discovery drops non-`claude-` ids. Use `ANTHROPIC_CUSTOM_MODEL_OPTION` (§7.1) or a `claude-`-named discovery alias (§7.2). |

Validate config before running: `cargo run -- check` (or `./target/release/shunt check`).

---

## 14. End-to-end example

`shunt.toml`:

```toml
[server]
bind = "127.0.0.1:3001"
default_provider = "anthropic"

# codex is built in; this table only pins a default effort and keeps local counting.
[providers.codex]
effort = "high"
# count_tokens = "tiktoken"   # default

[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
```

Shell (the environment shunt and Claude Code run in):

```bash
# One-time: log in so ~/.codex/auth.json exists
codex login

# Run the gateway
./target/release/shunt run

# In the Claude Code environment
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"   # add gpt-5.6-sol to the picker
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1            # let the effort slider reach Codex
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000         # gpt-5.6-sol's real window
```

Then pick **gpt-5.6-sol** from `/model`. Everything else in the session still flows to Anthropic
unchanged; only the mapped model's inference is answered by your ChatGPT/Codex subscription.

---

**See also:** [`running.md`](running.md) (full gateway workflow) ·
[`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md) (credential spec) ·
[`m1-responses-translation.md`](m1-responses-translation.md) (Anthropic ↔ Responses translation) ·
[`m3-discovery.md`](m3-discovery.md) (model discovery) ·
[`plugins/shunt-codex/`](../plugins/shunt-codex/) (ready-made GPT-5.6 subagents).
