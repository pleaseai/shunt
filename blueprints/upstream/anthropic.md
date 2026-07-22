# shunt blueprint: Anthropic

You are a coding agent adding Anthropic to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Choose either the operator's own Anthropic API credential (passthrough) or a Claude subscription account that shunt will manage with OAuth.

## Locate the config

Honor an explicit `--config` path first. Otherwise, shunt probes `shunt.toml`, then `shunt.yaml`, then `shunt.yml` in each of these locations: the current directory, `$XDG_CONFIG_HOME/shunt/` (normally `~/.config/shunt/`), and Homebrew's `etc/` directory. Edit the file already used by the deployment. If none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `anthropic` preset supplies `kind = "anthropic"`, `base_url = "https://api.anthropic.com"`, and `auth = "passthrough"`:

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"
```

Explicit fields override preset defaults.

## Credentials

The default `passthrough` mode forwards the client's own Anthropic credential. No server-side secret belongs in the config.

For a shunt-managed Claude OAuth pool instead, create a refreshable account and scope this upstream to it:

```bash
shunt login claude --name primary --mode oauth
```

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"
auth = { mode = "claude_oauth", account = "primary" }
```

Omit `account` to scan the whole shunt-managed Claude account store. Do not share a refreshable credential file between concurrently running shunt processes.

## Optional model routing

Expose and route a model with a per-upstream `upstream_model` map:

```toml
[[models]]
id = "claude-sonnet-5"
display_name = "Claude Sonnet 5"

[models.upstream_model]
anthropic = "claude-sonnet-5"
```

The map key is the `[[upstreams]].name`, not the preset id. A legacy exact `[[routes]]` entry may instead set `provider = "anthropic"` and an optional `upstream_model`, but do not define both forms for the same model.

## Validate

Run:

```bash
shunt check
```

Do not continue until it prints exactly `config ok`.

## Verify live

> These checks assume the gateway's defaults: it listens on `127.0.0.1:3001` with no `[server.auth]`, you reuse any explicit `--config` from the steps above, and the model entry below is discoverable. If the deployment differs, adjust the URL, send the configured inbound client token, pass the same `--config`, or use a model id explicitly routed to `anthropic`.

Start the gateway with `shunt run`, then inspect discovery:

```bash
curl -sS http://127.0.0.1:3001/v1/models
```

Confirm the expected model entry is present. For passthrough auth, send one minimal request with the operator's key without printing it:

```bash
curl -sS http://127.0.0.1:3001/v1/messages \
  -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-sonnet-5","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

For `claude_oauth`, omit the `x-api-key` header. Confirm the response is successful and was routed through `anthropic`.

## Safety rules

- Never print, log, or commit secrets.
- Keep API keys in environment variables, never in TOML or YAML.
- Preserve all unrelated config entries and security controls.
- Make the smallest reversible edit, validate it, and report exactly what changed.
