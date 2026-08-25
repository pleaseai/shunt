# shunt blueprint: Kimi Code subscription

You are a coding agent adding a Kimi Code subscription to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has a Kimi Code subscription (not the metered Moonshot API).
- Do not confuse this OAuth subscription upstream with the separately billed `kimi` (Moonshot) API-key preset — different service, different host, different credential.

## Locate the config

Honor an explicit `--config` path first. Otherwise, shunt probes `shunt.toml`, then `shunt.yaml`, then `shunt.yml` in the current directory, `$XDG_CONFIG_HOME/shunt/` (normally `~/.config/shunt/`), and Homebrew's `etc/` directory. Edit the active file. If none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `kimi-code` preset supplies `kind = "anthropic"`, `base_url = "https://api.kimi.com/coding"`, and `auth = "kimi_oauth"`:

```toml
[[upstreams]]
name = "kimi-code"
provider = "kimi-code"

# Declaring [[upstreams]] replaces the built-in provider set, so keep a trailing
# anthropic passthrough — without it `shunt check` rejects the default
# server.default_provider. This is the same entry `shunt init` appends.
[[upstreams]]
name = "anthropic"
provider = "anthropic"
```

Kimi Code uses shunt's Anthropic adapter because it speaks the Anthropic Messages wire shape directly. Explicit fields override preset defaults.

## Credentials

Run the device-code login and complete it in a browser (the RFC 8628 device authorization grant — shunt prints a URL and a short user code; approval can happen on another device, and shunt polls until it completes):

```bash
shunt login kimi --name <account-name>
```

`--name` is required. `--mode`, `--long-lived`, and `--manual` are not accepted for `shunt login kimi`: the token is always refreshable and there is no manual-paste fallback. shunt stores and refreshes the credential outside the config, at `~/.shunt/accounts/kimi/<account-name>.json` (override the directory with `SHUNT_KIMI_ACCOUNTS_DIR`). Never copy its contents into TOML or YAML.

`kimi_oauth` is pool-capable, exactly like `claude_oauth` and `chatgpt_oauth`: scope an upstream to one stored account or several.

```toml
[[upstreams]]
name = "kimi-code"
provider = "kimi-code"
auth = { mode = "kimi_oauth", account = "<account-name>" }

# Declaring [[upstreams]] replaces the built-in provider set, so keep a trailing
# anthropic passthrough — without it `shunt check` rejects the default
# server.default_provider. This is the same entry `shunt init` appends.
[[upstreams]]
name = "anthropic"
provider = "anthropic"
```

Use `accounts = [...]` instead of `account` to pool several named accounts under one upstream; omit both to scan the whole shunt-managed Kimi account store. Do not set both in the same map. Do not share a refreshable credential file between concurrently running shunt processes.

## Optional model routing

```toml
[[models]]
id = "claude-kimi-code"
display_name = "Kimi Code"

[models.upstream_model]
kimi-code = "<model-id-your-subscription-exposes>"
```

The map key is the upstream name. shunt does not query Kimi Code's own `/v1/models`; use an id your subscription is actually entitled to serve, and verify it against the operator's account rather than assuming one. A legacy exact `[[routes]]` entry may instead use `provider = "kimi-code"`; do not define both forms for one model.

## Validate

Run:

```bash
shunt check
```

Do not continue until it prints exactly `config ok`.

## Verify live

> These checks assume the gateway's defaults: it listens on `127.0.0.1:3001` with no `[server.auth]`, you reuse any explicit `--config` from the steps above, and the optional model-routing block was applied. If the deployment differs, adjust the URL, send the configured inbound client token, pass the same `--config`, or use a model id explicitly routed to `kimi-code`.

Start `shunt run`, then inspect discovery:

```bash
curl -sS http://127.0.0.1:3001/v1/models
```

Send one minimal request:

```bash
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-kimi-code","max_tokens":256,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm a successful response and that the selected upstream is `kimi-code`. Raise `max_tokens` if a reasoning model consumes the budget before emitting the reply.

## Safety rules

- Never print, log, or commit OAuth tokens or credential files.
- Keep `kimi-code` and `kimi` separate; their credentials, hosts, and billing are not interchangeable.
- Preserve all unrelated config entries and security controls.
- Make the smallest reversible edit, validate it, and report exactly what changed.
