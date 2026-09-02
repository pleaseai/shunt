# shunt blueprint: Antigravity subscription

You are a coding agent adding Google Antigravity to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has an Antigravity subscription. The credential is not interchangeable with a Gemini CLI login: Antigravity's OAuth flow requests two scopes (`cclog`, `experimentsandconfigs`) that `~/.gemini/oauth_creds.json` never carries, and `google_oauth` cannot stand in for `antigravity_oauth`.
- Do not confuse this with `antigravity-cli`. That built-in provider (`kind = "antigravity_cli"`) runs the local `agy` binary as a subprocess, is deprecated, and executes arbitrary code as the user running shunt. This blueprint configures the native HTTP upstream, which needs none of that.

## Locate the config

Honor an explicit `--config` path first. Otherwise, shunt probes `shunt.toml`, then `shunt.yaml`, then `shunt.yml` in the current directory, `$XDG_CONFIG_HOME/shunt/` (normally `~/.config/shunt/`), and Homebrew's `etc/` directory. Edit the active file. If none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

There is **no `antigravity` preset**, so an ordered upstream must set `kind`, `base_url`, and `auth` explicitly:

```toml
[[upstreams]]
name = "antigravity"
kind = "antigravity"
base_url = "https://daily-cloudcode-pa.googleapis.com"
auth = "antigravity_oauth"
```

Ordered `[[upstreams]]` replace shunt's built-in providers. If the config uses this form, also declare the Anthropic default that unrouted models still fall back to (`server.default_provider` defaults to `anthropic`):

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"
```

Do not mix `[[upstreams]]` with the legacy `[providers.*]` table form in one file. `base_url` must stay HTTPS on `daily-cloudcode-pa.googleapis.com` (the default, and the `daily-` control plane the Antigravity client itself addresses for both discovery and inference) or on `cloudcode-pa.googleapis.com` — no other `googleapis.com` host passes validation, `daily-cloudcode-pa.sandbox.googleapis.com` included (a loopback host is allowed for a local proxy). Anything else is refused at validation rather than sending the subscription token off-origin.

## Credentials

Run the Antigravity OAuth flow once:

```bash
shunt login antigravity
```

This writes `~/.shunt/antigravity-auth.json` (override the path with `SHUNT_ANTIGRAVITY_AUTH_FILE`) and resolves the Code Assist project during login, so no discovery sits in front of the first request. shunt refreshes the token itself. Never copy its token into TOML or YAML.

## Optional model routing

shunt keeps no model allowlist for this provider: the resolved `upstream_model` reaches the backend as written. Antigravity serves Gemini-family slugs the Code Assist `gemini` provider does not accept, and publishes them with the effort tier in the id — `-low`, `-medium`, or `-high` (`gemini-3.1-pro` has only `-low` and `-high`). Run `agy models` for the current catalog. A bare slug is not served: the daily host answers it with a `404`. Against the production host, shunt's earlier request — a bare id and the plain Code Assist envelope together — returned a misleading `429 RESOURCE_EXHAUSTED`, and the probes do not establish which of the two inputs caused it. shunt fills the suffix in for a bare `gemini-*` id — from `effort` on the route or provider, else the request's `output_config.effort`, else an enabled `thinking` budget, else `medium` — so either spelling works. `xhigh` and `max` fold onto `high` from either source.

Expose one through a Claude-named discovery alias:

```toml
[[models]]
id = "claude-gemini-3.6-flash-via-antigravity"
display_name = "[AGY ] Gemini-3.6-Flash"

[models.upstream_model]
antigravity = "gemini-3.6-flash-medium"
```

The map key is the upstream name. A legacy exact `[[routes]]` entry may instead use `provider = "antigravity"` and `upstream_model = "gemini-3.6-flash-medium"`; do not define both forms for one id.

Antigravity also offers Claude models, but shunt does not yet implement the request rewrites they need. Nothing rejects such a slug locally — it reaches the backend as written — so route only Gemini-family slugs.

## Validate

```bash
shunt check
```

Do not continue until it prints exactly `config ok`. This command also runs the routed-Antigravity credential guard offline: a route to `antigravity` with no stored credential fails here and names `shunt login antigravity`. The guard is presence-only, so an empty or stale credential still passes and fails later on the request path.

## Verify live

Start `shunt run`, then inspect discovery:

```bash
curl -sS http://127.0.0.1:3001/v1/models
```

Send one minimal request:

```bash
curl -sS http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-gemini-3.6-flash-via-antigravity","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm a successful response and that the selected upstream is `antigravity`.

## Safety rules

- Never print, log, or commit OAuth tokens or credential files.
- Keep credentials outside config.
- Never point `base_url` at any host other than `daily-cloudcode-pa.googleapis.com`, `cloudcode-pa.googleapis.com`, or a loopback proxy to "test" the upstream; validation refuses it because it would ship a subscription token off-origin.
- Preserve all unrelated config entries and security controls.
- Make the smallest reversible edit, validate it, and report exactly what changed.
