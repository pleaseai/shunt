---
title: Configuration
description: How shunt loads configuration — files, environment variables, and routing.
---

shunt loads configuration from, in increasing precedence:

1. **Built-in defaults** — every provider (`anthropic`, `openai`, `codex`, …) is preconfigured.
2. A **TOML file**. With `--config <path>` that exact file is used (a missing file is an error). Otherwise shunt takes the first file found in:
   - `./shunt.toml`
   - `$XDG_CONFIG_HOME/shunt/shunt.toml` (default `~/.config/shunt/shunt.toml`)
   - `$HOMEBREW_PREFIX/etc/shunt.toml` (default `/opt/homebrew` and `/usr/local` prefixes)

   Boot logs report which file was loaded, or that defaults are in use.
3. **Environment variables** prefixed `SHUNT_`, using `__` for nested keys — e.g. `SHUNT_SERVER__BIND=0.0.0.0:3001`.

Because the defaults already define every provider, your `shunt.toml` only needs the parts you want to change. Start from [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example).

## Annotated example

```toml
[server]
bind = "127.0.0.1:3001"        # address shunt listens on
default_provider = "anthropic" # provider for any model with no route (pass-through)

# Each provider is a [providers.<name>] table.
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
# effort = "high"

# Then prefix match.
[[route_prefixes]]
prefix = "gpt-"
provider = "openai"

# Optional: expose Claude-named aliases in the /model picker via discovery.
# The id MUST start with "claude" or "anthropic" or Claude Code ignores it.
# [[models]]
# id = "claude-opus-via-codex"
# display_name = "Opus (via Codex)"
```

## Routing precedence

1. Exact `[[routes]]` match on the request's `model` id.
2. `[[route_prefixes]]` prefix match.
3. `server.default_provider` — by default `anthropic`, so a model with no match falls through to Anthropic unchanged.

A route can override the forwarded model id (`upstream_model`) and the reasoning effort (`effort`) per model.

## Partial overrides

Config maps are deep-merged, so a partial override of a built-in provider keeps the rest of its defaults:

```toml
# Only raise codex's default effort; everything else stays at the built-in values.
[providers.codex]
effort = "high"
```

## Validate

```bash
shunt check
# -> prints "config ok", or a specific error (bad bind address, unknown provider, …)
```

See the [Configuration Reference](/reference/configuration/) for every key, and [Providers](/guides/providers/) for adding new backends.
