# shunt blueprint: API Route

You are a coding agent adding the API Route gateway to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has an API Route API key (https://api-route.com).

## Locate the config

Honor an explicit `--config` path first. Otherwise, edit the active `shunt.toml`, `shunt.yaml`, or `shunt.yml`; if none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `api-route` preset supplies `kind = "anthropic"`, `base_url = "https://global.api-route.com"`, and API-key auth from `API_ROUTE_API_KEY`:

```toml
[[upstreams]]
name = "api-route"
provider = "api-route"
```

When replacing the built-in provider set with `[[upstreams]]`, keep an `anthropic` passthrough upstream unless the operator intentionally changes `server.default_provider`.

Do not add `[[upstreams]]` to a file that already declares the legacy `[providers.*]` table form: `shunt check` rejects a file that mixes the two. In that case, stop and ask the operator whether to migrate the file to `[[upstreams]]` first.

## Credentials

Export the key in the environment that launches shunt:

```bash
export API_ROUTE_API_KEY='...'
```

Never write, print, log, or commit the key.

## Optional model routing

```toml
[[models]]
id = "claude-gpt-6-1-sol-via-api-route"
display_name = "GPT-6.1 Sol (via API Route)"
upstream_model = { api-route = "gpt-6.1-sol" }
```

Use an available model such as `gpt-6.1-sol`. Query `GET https://global.api-route.com/v1/models` with `Authorization: Bearer $API_ROUTE_API_KEY` to verify model availability for the operator's key. Do not assume every listed model supports Messages. Claude Code context-window hints belong on inbound model ids, not upstream model mappings.

## Validate

Run `shunt check` in the same environment as the key and continue only after it prints `config ok`. This only checks configuration structure, not credentials or model access. Start `shunt run`, send one minimal `/v1/messages` request for a routed model, and confirm `x-gateway-upstream` is `api-route`.

## Safety rules

- Keep credentials in the process environment or a secret manager.
- Preserve unrelated configuration and security controls.
- Make the smallest reversible edit and report exactly what changed.
