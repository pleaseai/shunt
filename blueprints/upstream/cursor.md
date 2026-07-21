# shunt blueprint: Cursor subscription

You are a coding agent adding Cursor to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has a Cursor subscription; free plans may only be entitled to the `default` wire model.

## Locate the config

Honor an explicit `--config` path first. Otherwise, shunt probes `shunt.toml`, then `shunt.yaml`, then `shunt.yml` in the current directory, `$XDG_CONFIG_HOME/shunt/` (normally `~/.config/shunt/`), and Homebrew's `etc/` directory. Edit the active file. If none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `cursor` preset supplies the native Cursor adapter, Cursor's backend URL, and `auth = "cursor_oauth"`:

```toml
[[upstreams]]
name = "cursor"
provider = "cursor"
```

Explicit fields override preset defaults.

## Credentials

Run Cursor OAuth once:

```bash
shunt login cursor
```

shunt stores and refreshes the credential outside the config. Never copy its token into TOML or YAML.

## Optional model routing

Cursor's Auto wire id is `default`, not `auto`. Expose it through a Claude-named discovery alias:

```toml
[[models]]
id = "claude-cursor-default"
display_name = "Cursor Auto"

[models.upstream_model]
cursor = "cursor:default"
```

The map key is the upstream name. The `cursor:`, `cursor-plan:`, and `cursor-ask:` prefixes select Agent, Plan, and Ask modes. A legacy exact `[[routes]]` entry may instead use `provider = "cursor"` and `upstream_model = "cursor:default"`; do not define both forms for one id.

## Validate

```bash
shunt check
```

Do not continue until it prints exactly `config ok`.

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
  -d '{"model":"claude-cursor-default","max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm a successful response and that the selected upstream is `cursor`. If `cursor:auto` appears anywhere, correct it to `cursor:default`.

## Safety rules

- Never print, log, or commit OAuth tokens or credential files.
- Keep credentials outside config.
- Preserve all unrelated config entries and security controls.
- Make the smallest reversible edit, validate it, and report exactly what changed.
