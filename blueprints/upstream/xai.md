# shunt blueprint: xAI API

You are a coding agent adding the metered xAI API to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has an xAI developer API key and access to the intended model.
- Do not confuse this API-key upstream with the separate `grok` subscription preset.

## Locate the config

Honor an explicit `--config` path first. Otherwise, shunt probes `shunt.toml`, then `shunt.yaml`, then `shunt.yml` in the current directory, `$XDG_CONFIG_HOME/shunt/` (normally `~/.config/shunt/`), and Homebrew's `etc/` directory. Edit the active file. If none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `xai` preset supplies `kind = "responses"`, `base_url = "https://api.x.ai/v1"`, and API-key auth from `XAI_API_KEY`:

```toml
[[upstreams]]
name = "xai"
provider = "xai"
```

Explicit fields override preset defaults.

## Credentials

Export the key only in the environment that launches shunt:

```bash
export XAI_API_KEY='...'
```

Never write the key into TOML or YAML.

## Optional model routing

```toml
[[models]]
id = "claude-grok-via-xai"
display_name = "Grok (via xAI API)"

[models.upstream_model]
xai = "grok-4.5"
```

The map key is the upstream name. Verify the current xAI model slug and entitlement. A legacy exact `[[routes]]` entry may instead use `provider = "xai"` with an `upstream_model`; do not define both forms for one id.

## Validate

```bash
shunt check
```

Do not continue until it prints exactly `config ok`.

## Verify live

> These checks assume the gateway's defaults: it listens on `127.0.0.1:3001` with no `[server.auth]`, you reuse any explicit `--config` from the steps above, and the optional model-routing block was applied. If the deployment differs, adjust the URL, send the configured inbound client token, pass the same `--config`, or use a model id explicitly routed to `xai`.

Start `shunt run`, inspect discovery, and confirm the alias appears:

```bash
curl -sS http://127.0.0.1:3001/v1/models
```

Then send one minimal request:

```bash
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-grok-via-xai","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm a successful response and that the selected upstream is `xai`.

## Safety rules

- Never print, log, or commit secrets.
- Keep `XAI_API_KEY` in the process environment or a secret manager, never in config.
- Keep `xai` and `grok` separate; their credentials and billing models are not interchangeable.
- Preserve all unrelated config entries and security controls.
