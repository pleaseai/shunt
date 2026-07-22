# shunt blueprint: ChatGPT/Codex

You are a coding agent adding the ChatGPT/Codex backend to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has a ChatGPT account entitled to a Codex model.

## Locate the config

Honor an explicit `--config` path first. Otherwise, shunt probes `shunt.toml`, then `shunt.yaml`, then `shunt.yml` in the current directory, `$XDG_CONFIG_HOME/shunt/` (normally `~/.config/shunt/`), and Homebrew's `etc/` directory. Edit the file already used by the deployment. If none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `codex` preset supplies the Responses adapter, the ChatGPT/Codex backend URL, and `auth = "chatgpt_oauth"`:

```toml
[[upstreams]]
name = "codex"
provider = "codex"
```

Explicit fields override preset defaults.

## Credentials

Import a ChatGPT OAuth account into shunt's managed store:

```bash
shunt login codex --name primary
```

The preset scans the whole Codex account store by default. To pin this upstream to one account, override the auth scope:

```toml
auth = { mode = "chatgpt_oauth", account = "primary" }
```

Do not share a refreshable credential file between concurrently running shunt processes.

## Optional model routing

Use a Claude-named public id for discovery and map it to an entitled Codex model:

```toml
[[models]]
id = "claude-gpt-5-6-sol-via-codex"
display_name = "GPT-5.6 Sol (via Codex)"

[models.upstream_model]
codex = "gpt-5.6-sol"
```

The map key is the upstream name. Confirm the operator's live account is entitled to the chosen upstream model. A legacy `[[routes]]` entry can instead use `provider = "codex"` and `upstream_model = "gpt-5.6-sol"`; do not define both exact-routing forms for one id.

## Validate

```bash
shunt check
```

Do not continue until it prints exactly `config ok`.

## Verify live

> These checks assume the gateway's defaults: it listens on `127.0.0.1:3001` with no `[server.auth]`, you reuse any explicit `--config` from the steps above, and the optional model-routing block was applied. If the deployment differs, adjust the URL, send the configured inbound client token, pass the same `--config`, or use a model id explicitly routed to `codex`.

Start `shunt run`, then confirm discovery:

```bash
curl -sS http://127.0.0.1:3001/v1/models
```

Send one minimal request through the mapped id:

```bash
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-gpt-5-6-sol-via-codex","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm a successful response and that the selected upstream is `codex`. If the backend rejects the model, discover an entitled slug before changing routing.

## Safety rules

- Never print, log, or commit OAuth tokens or account files.
- Never place credentials in the config.
- Preserve all unrelated config entries and security controls.
- Make the smallest reversible edit, validate it, and report exactly what changed.
