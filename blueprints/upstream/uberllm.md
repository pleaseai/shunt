# shunt blueprint: UberLLM

You are a coding agent adding the UberLLM gateway to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has an UberLLM API key (https://uberllm.dev/signup).

## Locate the config

Honor an explicit `--config` path first. Otherwise, edit the active `shunt.toml`, `shunt.yaml`, or `shunt.yml`; if none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `uberllm` preset supplies `kind = "anthropic"`, `base_url = "https://api.uberllm.dev"`, and API-key auth from `UBERLLM_API_KEY`:

```toml
[[upstreams]]
name = "uberllm"
provider = "uberllm"
```

When replacing the built-in provider set with `[[upstreams]]`, keep an `anthropic` passthrough upstream unless the operator intentionally changes `server.default_provider`.

Do not add `[[upstreams]]` to a file that already declares the legacy `[providers.*]` table form: `shunt check` rejects a file that mixes the two. In that case, stop and ask the operator whether to migrate the file to `[[upstreams]]` first.

## Credentials

Export the key in the environment that launches shunt:

```bash
export UBERLLM_API_KEY='...'
```

Never write, print, log, or commit the key.

## Optional model routing

```toml
[[models]]
id = "claude-deepseek-v4-1-flash-via-uberllm"
display_name = "DeepSeek V4.1 Flash (via UberLLM)"

[models.upstream_model]
uberllm = "deepseek/deepseek-v4.1-flash"
```

UberLLM serves models from many labs under `vendor/model` ids such as `deepseek/deepseek-v4.1-flash`, `anthropic/claude-sonnet-5`, or `openai/gpt-5.4-mini`. Verify the id against `https://api.uberllm.dev/v1/models` (public, no key) before routing. Claude Code appends its `[1m]` context-window hint to the inbound model id, never to `upstream_model` values; shunt strips the hint from inbound requests before route matching.

## Validate

Run `shunt check` in the same environment as the key and continue only after it prints `config ok`. Start `shunt run`, send one minimal `/v1/messages` request for a routed model, and confirm `x-gateway-upstream` is `uberllm`.

## Safety rules

- Keep credentials in the process environment or a secret manager.
- Preserve unrelated configuration and security controls.
- Make the smallest reversible edit and report exactly what changed.
