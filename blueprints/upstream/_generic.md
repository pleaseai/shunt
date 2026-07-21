# shunt blueprint: custom compatible upstream

You are a coding agent adding a custom endpoint to an operator's shunt gateway. Follow these steps; verify each before moving on.

Research starting point: {{RESEARCH_URL}}

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Read the research starting point and the endpoint's authoritative API and authentication documentation.
- Determine the available model ids and obtain an API key through the operator's approved secret-management process.

## Research the protocol first

Before editing config, determine which protocol the endpoint actually implements:

1. Choose `kind = "anthropic"` only if it accepts Anthropic Messages requests at a `/v1/messages`-style endpoint and returns Anthropic Messages/SSE shapes.
2. Choose `kind = "responses"` only if it accepts the OpenAI Responses API and returns Responses events. Do not select it merely because the provider also offers Chat Completions.
3. Record the documented base URL, whether shunt should append `/v1/messages` or `/responses`, the required authentication header, and one currently valid model id.
4. If neither existing kind fits, stop. Use `shunt add provider <documentation-url>` to retrieve the source-code implementation blueprint instead of inventing a config-only integration.

## Locate the config

Honor an explicit `--config` path first. Otherwise, shunt probes `shunt.toml`, then `shunt.yaml`, then `shunt.yml` in each of these locations: the current directory, `$XDG_CONFIG_HOME/shunt/` (normally `~/.config/shunt/`), and Homebrew's `etc/` directory. Edit the active file. If none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

Use a short stable name. Without a built-in preset, `kind` and `base_url` are required. Put the key in a purpose-specific environment variable:

```toml
[[upstreams]]
name = "custom"
kind = "anthropic" # or "responses", based on verified protocol evidence
base_url = "https://provider.example/api"
auth = { mode = "api_key", env = "CUSTOM_API_KEY" }
```

If the provider requires Anthropic's `x-api-key` header instead of bearer auth, add the documented header override to the auth map using shunt's supported header value. Do not guess. Explicit fields define the entire custom upstream because no preset supplies defaults.

## Credentials

Export the key in the environment that launches shunt:

```bash
export CUSTOM_API_KEY='...'
```

Never paste the value into config, commands captured by logs, issue text, or version control.

## Optional model routing

Expose a Claude-named alias to discovery and map it to the verified upstream model id:

```toml
[[models]]
id = "claude-custom-model"
display_name = "Custom model"

[models.upstream_model]
custom = "provider-model-id"
```

The map key must exactly match `[[upstreams]].name`. A legacy exact `[[routes]]` entry may instead use `provider = "custom"` and an `upstream_model`; do not define both forms for one model.

## Validate

Run in the same environment as the key:

```bash
shunt check
```

Do not continue until it prints exactly `config ok`.

## Verify live

Start `shunt run`, then inspect discovery:

```bash
curl -sS http://127.0.0.1:3001/v1/models
```

Confirm the expected alias. Then send one minimal Anthropic Messages request to shunt, regardless of the upstream protocol—shunt performs Responses translation when configured:

```bash
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-custom-model","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm a successful response, the expected upstream selection, streamed output when requested, and a correctly shaped error from one deliberately invalid model id. If verification contradicts the protocol decision, stop and revise from documented wire evidence rather than layering workarounds.

## Safety rules

- Never print, log, or commit secrets.
- Keep API keys in environment variables or a secret manager, never in config.
- Do not send a credential to an unverified origin; confirm HTTPS and the exact hostname.
- Preserve all unrelated config entries and security controls.
- Make the smallest reversible edit, validate it, and report the evidence and exact changes.
