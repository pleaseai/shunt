# shunt blueprint: OpenCode Zen

You are a coding agent adding the OpenCode Zen endpoint to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has an OpenCode Zen API key (opencode console → API key, or the `opencode` CLI login key at `~/.local/share/opencode/auth.json`).

## Locate the config

Honor an explicit `--config` path first. Otherwise, edit the active `shunt.toml`, `shunt.yaml`, or `shunt.yml`; if none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `opencode` preset supplies `kind = "anthropic"`, `base_url = "https://opencode.ai/zen"`, API-key auth from `OPENCODE_API_KEY`, and the `x-api-key` header the zen endpoint reads:

```toml
[[upstreams]]
name = "opencode"
provider = "opencode"
```

When replacing the built-in provider set with `[[upstreams]]`, keep an `anthropic` passthrough upstream unless the operator intentionally changes `server.default_provider`.

## Credentials

Export the key in the environment that launches shunt:

```bash
export OPENCODE_API_KEY='...'
```

Never write, print, log, or commit the key.

## Optional model routing

```toml
[[models]]
id = "claude-fable-5-via-zen"
display_name = "Claude Fable 5.1 (via OpenCode Zen)"

[models.upstream_model]
opencode = "claude-fable-5-1"
```

Zen serves a cross-vendor catalog — Claude, GPT, GLM, Grok and more — under vendor-suffixed ids like `claude-fable-5-1`, `gpt-6-astra`, or `z-ai/glm-5.3`-style ids on the aggregator surface; the Anthropic endpoint takes the unsuffixed ids from `https://opencode.ai/zen/v1/models`. Verify the id against the live list before routing. Claude Code appends its `[1m]` context-window hint to the inbound model id, never to `upstream_model` values; shunt strips the hint from inbound requests before route matching.

## Validate

Run `shunt check` in the same environment as the key and continue only after it prints `config ok`. Start `shunt run`, send one minimal `/v1/messages` request for a routed model, and confirm `x-gateway-upstream` is `opencode`.

## Safety rules

- Keep credentials in the process environment or a secret manager.
- Preserve unrelated configuration and security controls.
- Make the smallest reversible edit and report exactly what changed.
