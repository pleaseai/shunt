# shunt blueprint: Moonshot Kimi

You are a coding agent adding Moonshot Kimi to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has a Moonshot API key and access to the intended Kimi model.

## Locate the config

Honor an explicit `--config` path first. Otherwise, shunt probes `shunt.toml`, then `shunt.yaml`, then `shunt.yml` in the current directory, `$XDG_CONFIG_HOME/shunt/` (normally `~/.config/shunt/`), and Homebrew's `etc/` directory. Edit the active file. If none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `kimi` preset supplies `kind = "anthropic"`, `base_url = "https://api.moonshot.ai/anthropic"`, and API-key auth from `MOONSHOT_API_KEY`:

```toml
[[upstreams]]
name = "kimi"
provider = "kimi"
```

Kimi uses shunt's Anthropic adapter because Moonshot exposes an Anthropic Messages-compatible endpoint. Explicit fields override preset defaults.

## Credentials

Export the official Moonshot variable in the environment that launches shunt:

```bash
export MOONSHOT_API_KEY='...'
```

Never write the key into the config. Older local examples may use `KIMI_API_KEY`; either migrate the environment variable or explicitly set `auth = { mode = "api_key", env = "KIMI_API_KEY" }` while preserving secret separation.

## Optional model routing

```toml
[[models]]
id = "claude-kimi-via-moonshot"
display_name = "Kimi (via Moonshot)"

[models.upstream_model]
kimi = "kimi-k2.7-code"
```

The map key is the upstream name. Verify Moonshot's current model catalog before choosing the upstream id. Do not put Claude Code's `[1m]` context hint in an `upstream_model` value. A legacy exact `[[routes]]` entry may instead use `provider = "kimi"`; do not define both forms for one model.

## Validate

Run in the same environment as the key:

```bash
shunt check
```

Do not continue until it prints exactly `config ok`.

## Verify live

> These checks assume the gateway's defaults: it listens on `127.0.0.1:3001` with no `[server.auth]`, you reuse any explicit `--config` from the steps above, and the optional model-routing block was applied. If the deployment differs, adjust the URL, send the configured inbound client token, pass the same `--config`, or use a model id explicitly routed to `kimi`.

Start `shunt run`, then inspect discovery:

```bash
curl -sS http://127.0.0.1:3001/v1/models
```

Send one minimal request:

```bash
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-kimi-via-moonshot","max_tokens":256,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm a successful response and that the selected upstream is `kimi`. Raise `max_tokens` if a reasoning model consumes the budget before emitting the reply.

## Safety rules

- Never print, log, or commit secrets.
- Keep `MOONSHOT_API_KEY` in the process environment or a secret manager, never in config.
- Preserve all unrelated config entries and security controls.
- Make the smallest reversible edit, validate it, and report exactly what changed.
