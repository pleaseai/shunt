---
title: ChatGPT / Codex
description: Route Claude Code inference to your ChatGPT/Codex subscription by reusing ~/.codex/auth.json — auth, model slugs, effort, and context window.
---

The **`codex`** provider routes a mapped model's inference to your **ChatGPT / Codex
subscription** instead of an API key. It reuses the credential the Codex CLI already wrote to
`~/.codex/auth.json`, so there's nothing to paste and no per-token billing — the request is
authenticated as your ChatGPT account and answered by the same backend the `codex` CLI talks to.

This page is the end-to-end setup. It links out to the deeper topic pages
([Effort & Context](/guides/effort-and-context/), [Model Discovery](/guides/model-discovery/),
[Providers](/guides/providers/)) rather than repeating them.

## How it works

`codex` is a built-in **`kind = "responses"`** provider: shunt translates Claude Code's Anthropic
Messages request into the OpenAI **Responses API**, sends it to the ChatGPT-account Codex backend,
and translates the streamed reply back. Three things make it "Codex" rather than plain OpenAI:

| Aspect | Value |
| :-- | :-- |
| Endpoint | `<base_url>/codex/responses` |
| Auth | ChatGPT OAuth from `~/.codex/auth.json`, auto-refreshed |
| Responses dialect | `Chatgpt` flavor — drops params codex never sends (e.g. `max_output_tokens`), sends `store: false`, round-trips encrypted reasoning |

The dialect is keyed on `auth = "chatgpt_oauth"`, not the provider name.

## 1. Log in

Log in once with the Codex CLI. shunt reads and refreshes the file it writes — it does **not**
run its own login for Codex.

```bash
codex login
```

This creates `~/.codex/auth.json`. If that file is missing, has no tokens, or its refresh token
is gone, shunt returns an `authentication_error` telling you to run `codex login` again.

:::note[A different auth-file location]
shunt looks at `$CODEX_AUTH_FILE` first, then `$HOME/.codex/auth.json`, then `.codex/auth.json`.
Point it elsewhere for CI, a sandbox, or a second account:

```bash
export CODEX_AUTH_FILE=/etc/shunt/codex-auth.json
```
:::

## 2. The provider block (optional)

`codex` is built in — you don't need to declare it. This is the full default; a partial table
overrides only the keys you set (config maps deep-merge):

```toml
[providers.codex]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"   # shunt appends /codex/responses
auth = "chatgpt_oauth"                          # read + auto-refresh ~/.codex/auth.json
# effort = "high"                               # optional default reasoning effort (§4)
# count_tokens = "tiktoken"                      # default; "estimate" opts out
```

Common overrides: pin a default `effort` for all Codex traffic, or set
`count_tokens = "estimate"`. `api_key_env` / `api_key_header` don't apply to `chatgpt_oauth` —
the credential comes from the auth file. See the [Configuration Reference](/reference/configuration/#providersname)
for every key.

:::note[ApiKey mode goes to the `openai` provider]
If `~/.codex/auth.json` is in **`ApiKey`** mode (you logged in with an OpenAI API key, not a
ChatGPT account), the `codex` OAuth path finds no tokens and errors. That key is instead picked
up by the **`openai`** provider as a fallback when `OPENAI_API_KEY` is unset. `codex` is
specifically the ChatGPT-subscription path.
:::

## 3. Route a model to `codex`

A request's `model` id picks the provider. Precedence: exact `[[routes]]` →
`[[route_prefixes]]` → `server.default_provider`.

```toml
[[routes]]
model = "gpt-5.6-sol"        # the id Claude Code sends (see §4 below)
provider = "codex"
# upstream_model = "gpt-5.6-sol"   # optional: forward a different slug upstream
# effort = "high"                  # optional: pin effort for this route
```

`upstream_model` lets the id Claude Code sends differ from the slug the backend receives — the
mechanism behind [discovery aliases](/guides/model-discovery/) and a way to swap the real slug
without touching your Claude Code env.

:::caution[Model slugs — no `-codex`]
The ChatGPT-account backend **rejects** `gpt-*-codex` slugs (e.g. `gpt-5.2-codex`) with a `400`;
it only accepts your account's **live-entitled** slugs. The authoritative catalog (and the
reasoning levels each accepts) is openai/codex's
[`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json).
Current slugs: `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna` (frontier) and
`gpt-5.5` / `gpt-5.4` / `gpt-5.4-mini` / `gpt-5.2`. Older accounts may only be entitled to the
earlier ones (a free account has resolved to `gpt-5.5`). shunt surfaces the backend's own error
`detail`, so a wrong slug returns the real reason.
:::

:::note[`Model not found <slug>` is client-version gating, not entitlement]
Some slugs carry a `minimal_client_version` (e.g. `gpt-5.6-luna` needs ≥ 0.144.0). When the
request's client identity is missing or too old the backend answers `Model not found <slug>`.
shunt avoids this by sending the pinned Codex CLI identity headers (`originator: codex_cli_rs`,
`user-agent`, `version`), pinned to **openai/codex rust-v0.144.1**. See
[openai/codex#31967](https://github.com/openai/codex/issues/31967).
:::

## 4. Select the model in Claude Code

Claude Code's `/model` picker only honors discovery ids beginning with `claude`/`anthropic`, so a
raw `gpt-*` id needs one of two paths — they split on the `claude-` prefix and don't overlap:

| | `claude-…` discovery alias | non-`claude-` id (`gpt-5.6-sol`) |
| :-- | :-- | :-- |
| `/model` picker via discovery | ✅ auto-listed, many models | ❌ dropped by Claude Code |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | ❌ not honored | ✅ adds to picker (one id) |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` window | ❌ ignored → 200k | ✅ real window |

**Primary path** — add the slug to the picker directly:

```bash
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"
```

That id is exactly what shunt routes on, so it must match a `[[routes]]`/`[[route_prefixes]]`
rule. This is the recommended path — it's the only one that also lets you set an accurate context
window. For auto-listing several Codex models in the picker instead, use a `claude-`-named
[discovery alias](/guides/model-discovery/) (accepting the 200k window trade-off).

#### Put a subagent on a Codex slug

A subagent can run on a Codex slug while the main session stays on Claude. The `model:` frontmatter
field accepts **any string** (unlike the Agent/Task tool's `model` parameter, which only takes the
built-in aliases). To point an **existing** subagent at `gpt-5.6-sol`, edit its
`.claude/agents/<name>.md` and set `model:`:

```markdown
---
name: researcher
description: Deep research agent.
model: gpt-5.6-sol        # was: sonnet (or absent → inherited)
---

<the agent's system prompt — unchanged>
```

Spawn it **without** a `model` override (the tool parameter outranks frontmatter). Resolution order:
`CLAUDE_CODE_SUBAGENT_MODEL` > tool `model` > frontmatter > `inherit`. To force **every** subagent
onto one slug, set `export CLAUDE_CODE_SUBAGENT_MODEL="gpt-5.6-sol"`.

Either way the slug needs a `[[routes]]` entry and, being non-`claude-`, obeys
`CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` and `CLAUDE_CODE_MAX_CONTEXT_TOKENS` — the window follows the id
automatically.

:::tip[Ready-made agents]
The **[`shunt-codex` plugin](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-codex)**
ships subagents for `gpt-5.6-sol` / `-terra` / `-luna` — install with
`/plugin install shunt-codex@shunt` after `/plugin marketplace add pleaseai/shunt`.
:::

### Remap the tier aliases to Codex

Instead of adding one custom id, repoint Claude Code's **built-in tier aliases** at Codex slugs, so
the whole session's tier system resolves to your ChatGPT subscription
([model-config env vars](https://code.claude.com/docs/en/model-config#environment-variables)).

| Env var | Controls |
| :-- | :-- |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | the `haiku` alias **and the background "small-fast" model** |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | the `sonnet` alias |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` / `ANTHROPIC_DEFAULT_FABLE_MODEL` | the `opus` / `fable` aliases |

A two-tier setup — `haiku → gpt-5.6-luna`, `sonnet → gpt-5.6-sol`:

```bash
export ANTHROPIC_DEFAULT_HAIKU_MODEL="gpt-5.6-luna"
export ANTHROPIC_DEFAULT_SONNET_MODEL="gpt-5.6-sol"

# nicer picker labels (the _NAME/_DESCRIPTION companions work on a gateway)
export ANTHROPIC_DEFAULT_SONNET_MODEL_NAME="GPT-5.6-Sol"
export ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION="ChatGPT/Codex Sol via shunt"
export ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME="GPT-5.6-Luna"
export ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION="ChatGPT/Codex Luna via shunt (background tier)"
```

```toml
# shunt.toml — both resolved ids need a route
[[routes]]
model = "gpt-5.6-luna"
provider = "codex"

[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
```

Selecting **Sonnet** in `/model` now runs `gpt-5.6-sol` via Codex, and every background/haiku task
runs `gpt-5.6-luna` — the resolved id is exactly what shunt routes on, so no
`ANTHROPIC_CUSTOM_MODEL_OPTION` is needed.

:::note[Getting it right]
- The resolved ids don't start with `claude-`, so set `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` for the
  effort dial. `gpt-5.6-sol` and `gpt-5.6-luna` are **both 372k**, so one global
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000` fits both tiers.
- The `_SUPPORTED_CAPABILITIES` companion is documented for third-party providers (Bedrock, …), not
  confirmed for gateways — on shunt use `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` for effort.
- The **haiku tier is the background "small-fast" model** (summaries, titles, quick classification).
  Routing it to a reasoning model is fine, but it spends ChatGPT quota on that frequent traffic and
  can be slower — pick your cheapest entitled slug there if that matters.
- Remapping is **global and session-wide**; with an allowlist (`availableModels` /
  `enforceAvailableModels`) an alias can't be redirected outside the list (Claude Code enforces this
  on the tier-alias env vars as of **v2.1.176**).
:::

## 5. Reasoning effort

Set the effort with Claude Code's usual controls (`/effort`, the `/model` slider, `--effort`).
shunt maps it to the Responses `reasoning.effort`, folding `max → xhigh` for slugs that don't
support `max` (only the **gpt-5.6** family does).

:::note[Required for custom ids]
For an id Claude Code doesn't recognize as effort-capable (like `gpt-5.6-sol`) you must set:

```bash
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1
```

Otherwise Claude Code omits the effort field and shunt falls back to `medium`. A config
`route.effort` / `[providers.codex].effort` override wins over the client value.
:::

Full precedence and the effort table: [Effort & Context](/guides/effort-and-context/#reasoning-effort).

## 6. Context window

Claude Code sizes its context bar at a fixed **200k** for mapped ids. `gpt-5.6-sol`'s real window
is **372k** (`gpt-5.5` is 272k), so raise it for a non-`claude-` id:

```bash
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000
```

It's **global** (one value per session) and setting it larger than the real window causes
`prompt is too long` overflow churn — match it to the smallest real window among your mapped
models. shunt rewrites that overflow so Claude Code auto-compacts and retries, but each round-trip
is wasted latency. Details, the live-verified boundary, and `count_tokens` behavior:
[Effort & Context](/guides/effort-and-context/#context--usage-display-for-mapped-models).

## Full example

`shunt.toml`:

```toml
[server]
bind = "127.0.0.1:3001"
default_provider = "anthropic"

[providers.codex]
effort = "high"     # optional: pin high effort for all Codex traffic

[[routes]]
model = "gpt-5.6-sol"
provider = "codex"
```

Shell (both shunt and Claude Code run with these):

```bash
codex login                                          # one-time
./target/release/shunt run                           # start the gateway

export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.6-sol"   # add to /model picker
export CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1            # let the effort slider reach Codex
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=372000         # gpt-5.6-sol's real window
```

Pick **gpt-5.6-sol** from `/model`. Everything else in the session still flows to Anthropic
unchanged; only the mapped model's inference is answered by your ChatGPT/Codex subscription.

## Troubleshooting

| Symptom | Cause / Fix |
| :-- | :-- |
| `ChatGPT auth not found; run codex login` | No `~/.codex/auth.json` (or wrong `$CODEX_AUTH_FILE`). Run `codex login`. |
| `ChatGPT auth tokens missing` | Auth file is in `ApiKey` mode — that's the `openai` provider. Re-`codex login` with a ChatGPT account. |
| `400 … not supported when using Codex with a ChatGPT account` | You used a `gpt-*-codex` slug. Use an entitled non-`-codex` slug. |
| `Model not found <slug>` | Client-version gating or an unentitled slug — confirm via `models.json`. |
| Effort slider ignored on a `gpt-*` id | Set `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1`, or a route/provider `effort` override is winning. |
| Context bar over-reports / compacts early | Set `CLAUDE_CODE_MAX_CONTEXT_TOKENS`; a discovery alias can't take it — use a non-`claude-` id. |

See the full [Troubleshooting](/reference/troubleshooting/) reference for more.
