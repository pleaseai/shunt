# shunt blueprint: OpenAI

You are a coding agent adding OpenAI to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has an OpenAI API key and access to the intended Responses API model.

## Locate the config

Honor an explicit `--config` path first. Otherwise, shunt probes `shunt.toml`, then `shunt.yaml`, then `shunt.yml` in the current directory, `$XDG_CONFIG_HOME/shunt/` (normally `~/.config/shunt/`), and Homebrew's `etc/` directory. Edit the file already used by the deployment. If none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `openai` preset supplies `kind = "responses"`, `base_url = "https://api.openai.com/v1"`, and API-key auth from `OPENAI_API_KEY`:

```toml
[[upstreams]]
name = "openai"
provider = "openai"
```

Explicit fields override preset defaults.

## Credentials

Export the key in the environment that launches shunt:

```bash
export OPENAI_API_KEY='...'
```

Never write the key into the config. For a service manager, put the variable in its secret facility rather than a shell history or repository file.

## Optional model routing

```toml
[[models]]
id = "claude-gpt-5-4-via-openai"
display_name = "GPT-5.4 (via OpenAI)"

[models.upstream_model]
openai = "gpt-5.4"
```

The map key is the upstream name. Verify current model availability before selecting a slug. A legacy exact `[[routes]]` entry may instead use `provider = "openai"` and `upstream_model = "gpt-5.4"`; do not define both forms for the same model.

## Validate

Run `shunt check` in the same environment as the key:

```bash
shunt check
```

Do not continue until it prints exactly `config ok`.

## Verify live

> These checks assume the gateway's defaults: it listens on `127.0.0.1:3001` with no `[server.auth]`, you reuse any explicit `--config` from the steps above, and the optional model-routing block was applied (the request below uses `claude-gpt-5-4-via-openai` from that block). If the deployment differs, adjust the URL, send the configured inbound client token, pass the same `--config`, or use a model id explicitly routed to `openai`.

Start `shunt run`, then inspect discovery:

```bash
curl -sS http://127.0.0.1:3001/v1/models
```

Send one minimal request:

```bash
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-gpt-5-4-via-openai","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm a successful response and that the selected upstream is `openai`.

## Safety rules

- Never print, log, or commit secrets.
- Keep `OPENAI_API_KEY` in the process environment or a secret manager, never in config.
- Preserve all unrelated config entries and security controls.
- Make the smallest reversible edit, validate it, and report exactly what changed.
