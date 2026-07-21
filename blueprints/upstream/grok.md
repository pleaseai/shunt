# shunt blueprint: Grok subscription

You are a coding agent adding a SuperGrok or X Premium+ subscription to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has a subscription tier entitled to the Grok CLI proxy.
- Do not confuse this OAuth subscription upstream with the separately billed `xai` API-key preset.

## Locate the config

Honor an explicit `--config` path first. Otherwise, shunt probes `shunt.toml`, then `shunt.yaml`, then `shunt.yml` in the current directory, `$XDG_CONFIG_HOME/shunt/` (normally `~/.config/shunt/`), and Homebrew's `etc/` directory. Edit the active file. If none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `grok` preset supplies `kind = "responses"`, `base_url = "https://cli-chat-proxy.grok.com/v1"`, and `auth = "xai_oauth"`:

```toml
[[upstreams]]
name = "grok"
provider = "grok"
```

Explicit fields override preset defaults.

## Credentials

Run the device-code login and complete it in the operator's browser:

```bash
shunt login xai
```

shunt stores and refreshes the OAuth credential outside the config. Never copy its token into TOML or YAML. xAI may gate this backend by subscription tier; a 403 can mean the account is not entitled, in which case use the metered `xai` preset with an API key instead.

## Optional model routing

```toml
[[models]]
id = "claude-grok-via-subscription"
display_name = "Grok (subscription)"

[models.upstream_model]
grok = "grok-4.5"
```

The map key is the upstream name. Verify the currently entitled model slug. A legacy `[[routes]]` entry can instead use `provider = "grok"` and an `upstream_model`; do not define both exact-routing forms for one id.

## Validate

```bash
shunt check
```

Do not continue until it prints exactly `config ok`.

## Verify live

Start `shunt run`, then confirm discovery:

```bash
curl -sS http://127.0.0.1:3001/v1/models
```

Send one minimal request:

```bash
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-grok-via-subscription","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm a successful response and that the selected upstream is `grok`.

## Safety rules

- Never print, log, or commit OAuth tokens or credential files.
- Keep `grok` and `xai` separate; their credentials and billing models are not interchangeable.
- Preserve all unrelated config entries and security controls.
- Make the smallest reversible edit, validate it, and report exactly what changed.
