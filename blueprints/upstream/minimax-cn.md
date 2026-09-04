# shunt blueprint: MiniMax China

You are a coding agent adding MiniMax's China Anthropic-compatible endpoint to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has a MiniMax API key from the China open platform.

## Locate the config

Honor an explicit `--config` path first. Otherwise, edit the active `shunt.toml`, `shunt.yaml`, or `shunt.yml`; if none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `minimax-cn` preset supplies `kind = "anthropic"`, `base_url = "https://api.minimax.cn/anthropic"`, and API-key auth from `MINIMAX_API_KEY`:

```toml
[[upstreams]]
name = "minimax-cn"
provider = "minimax-cn"
```

When replacing the built-in provider set with `[[upstreams]]`, keep an `anthropic` passthrough upstream unless the operator intentionally changes `server.default_provider`.

## Credentials

Export the key in the environment that launches shunt:

```bash
export MINIMAX_API_KEY='...'
```

Never write, print, log, or commit the key. Do not reuse an international MiniMax key for the China endpoint.

## Optional model routing

```toml
[[models]]
id = "claude-minimax-cn"
display_name = "MiniMax (China)"

[models.upstream_model]
minimax-cn = "MiniMax-M3"
```

Remove Claude Code's `[1m]` context hint from `upstream_model`; shunt strips it from inbound requests before matching.

## Validate

Run `shunt check` in the same environment as the key and continue only after it prints `config ok`. Start `shunt run`, send one minimal `/v1/messages` request for a routed model, and confirm `x-gateway-upstream` is `minimax-cn`.

## Safety rules

- Keep credentials in the process environment or a secret manager.
- Preserve unrelated configuration and security controls.
- Make the smallest reversible edit and report exactly what changed.
