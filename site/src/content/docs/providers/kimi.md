---
title: Kimi (Moonshot)
description: Route mapped models to Moonshot's Anthropic-compatible Kimi endpoint with a MOONSHOT_API_KEY, or reuse a Kimi Code subscription via OAuth.
---

**Kimi** is Moonshot AI's model family, served over an **Anthropic-compatible** endpoint —
shunt injects the Moonshot API key and forwards Claude Code's Messages request. Non-Anthropic
upstream ids have deferred-tool fields stripped (same rule as OpenRouter stealth slugs). The
`kimi` preset is built in, so configuration is one upstream entry plus routes.

This page covers two separate Kimi services with separate credentials: the metered Moonshot
API (`kimi` preset, API key, below) and the **Kimi Code** subscription (`kimi-code` preset,
OAuth login, see [Kimi Code (OAuth subscription)](#kimi-code-oauth-subscription) at the
bottom of this page). They are different endpoints and are not interchangeable.

## Quick start

Let a coding agent wire it up for you — `shunt add` prints an embedded setup blueprint
(offline and read-only; the agent edits the config, the command never does):

```bash
shunt add upstream kimi --print | claude
```

Or follow the manual steps below.

## Configure the upstream

The `kimi` preset supplies `kind = "anthropic"`, `base_url = "https://api.moonshot.ai/anthropic"`,
and API-key auth from `MOONSHOT_API_KEY`:

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"   # keep Anthropic as the default for unrouted models (e.g. claude-*)

[[upstreams]]
name = "kimi"
provider = "kimi"

[[routes]]
model = "kimi-k3"
provider = "kimi"

[[routes]]
model = "kimi-k2.7-code"
provider = "kimi"
```

Ordered `[[upstreams]]` replace shunt's built-in providers, so the config that routes to `kimi`
must also declare the `anthropic` default it still points at (`server.default_provider` defaults
to `anthropic`); drop the `anthropic` entry only if you also set `default_provider` to a declared
upstream.

The legacy `[providers.kimi]` table form remains supported (older examples used
`api_key_env = "KIMI_API_KEY"`, which still works when set explicitly) — but do not mix
`[[upstreams]]` and `[providers.*]` in one file.

## Credentials

```bash
export MOONSHOT_API_KEY='...'
```

Never write the key into the config. `shunt check` validates the config's structure but does not
read the key's value — if `MOONSHOT_API_KEY` is unset, the first request routed to `kimi` returns
an authentication error.

## Models

| Model id | Notes |
| :-- | :-- |
| `kimi-k3` | frontier tier; a client may append Claude Code's `[1m]` context marker (`kimi-k3[1m]`) — shunt strips it before matching, so route the unsuffixed id |
| `kimi-k2.7-code` | coding-focused tier |

Select a routed id in Claude Code via `ANTHROPIC_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`, or a
subagent's `model:` frontmatter. To surface an entry in the `/model` picker instead, advertise a
`claude`-prefixed alias with a `[models.upstream_model]` map — see
[Model Discovery](/guides/model-discovery/).

## Verify

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"kimi-k2.7-code","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `kimi`, then
[point Claude Code at shunt](/guides/connect-claude-code/).

## Subagent plugin

The [`shunt-kimi` plugin](https://github.com/pleaseai/shunt/tree/main/plugins/shunt-kimi) ships
one ready-made Claude Code subagent per model above:

```bash
/plugin marketplace add pleaseai/shunt
/plugin install shunt-kimi@shunt
```

## Kimi Code (OAuth subscription)

**Kimi Code** is a separate, subscription-billed service from the metered Moonshot API above —
a different host (`api.kimi.com`, not `api.moonshot.ai`) and a different credential (an OAuth
token shunt manages, not `MOONSHOT_API_KEY`). It also speaks the Anthropic Messages wire shape,
so it uses the same adapter, just a different preset: `kimi-code`.

### Quick start

```bash
shunt add upstream kimi-code --print | claude
```

Or follow the manual steps below.

### 1. Log in

```bash
shunt login kimi --name <account-name>
```

`--name` is required; `--mode`, `--long-lived`, and `--manual` are not accepted for this login —
the credential is always refreshable and there is no manual-paste fallback. shunt runs an
[RFC 8628](https://www.rfc-editor.org/rfc/rfc8628) device authorization grant: it prints a URL
and a short code, you approve in a browser (on this device or another one), and shunt polls
until approval completes or the code expires. The stored account lands at
`~/.shunt/accounts/kimi/<account-name>.json` (0600, in a 0700 directory), overridable with
`SHUNT_KIMI_ACCOUNTS_DIR`.

Kimi rotates the refresh token on every refresh, and its access tokens last only about
15 minutes, so refreshes are frequent. Run one shunt process per Kimi account file — two
processes sharing a file will invalidate each other on the first refresh. Provision a
separate account for each process instead.

### 2. Configure the upstream

The `kimi-code` preset supplies `kind = "anthropic"`, `base_url = "https://api.kimi.com/coding"`,
and `auth = "kimi_oauth"`:

```toml
[[upstreams]]
name = "kimi-code"
provider = "kimi-code"
auth = { mode = "kimi_oauth", account = "<account-name>" }

# Declaring [[upstreams]] replaces the built-in provider set, so keep a trailing
# anthropic passthrough — without it `shunt check` rejects the default
# server.default_provider. This is the same entry `shunt init` appends.
[[upstreams]]
name = "anthropic"
provider = "anthropic"
```

`kimi_oauth` is pool-capable, exactly like `claude_oauth` and `chatgpt_oauth`: use
`accounts = [...]` instead of `account` to pool several named accounts under one upstream
(the two are mutually exclusive), or omit both to scan the whole shunt-managed Kimi account
store.

### Models

shunt does not query Kimi Code's own model-listing endpoint — it serves `/v1/models` from
shunt's built-in catalog. Route the model ids your subscription is actually entitled to:

```toml
[[routes]]
model = "<model-id-your-subscription-exposes>"
provider = "kimi-code"
```

### Verify

```bash
shunt check    # -> config ok
shunt run
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"<model-id-your-subscription-exposes>","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm the response's `x-gateway-upstream` header names `kimi-code`.

A `402 Payment Required` with `"We're unable to verify your membership benefits at this time"`
means the login worked but that account has no active Kimi Code membership. The credential is
fine; the subscription is what needs attention.

### Pooled accounts and the admin surface

A `kimi_oauth` pool participates in the same load-balancing, failover, and quota-aware account
rotation as the Claude and Codex pools, and its accounts appear in `GET /admin/pool` and in the
sanitized `GET /usage` aggregate when those are enabled. It rotates on one extra condition the
other pools do not have: the `402` membership response above. Because an inactive membership
returns 402 on every request, shunt treats it as an account-level failure — it cools that account
down and tries the next one, rather than handing the 402 to your client while healthy accounts sit
idle. If *every* account in the pool is inactive, you still get Kimi's own 402 status and message
back, so the cause stays visible. Browser-driven account provisioning in
the [admin web surface](https://shunt.dev/guides/admin-remote-provisioning/) does not support
Kimi accounts — that surface's pool view is read-only for Kimi; provision Kimi accounts with
`shunt login kimi` on the CLI.
