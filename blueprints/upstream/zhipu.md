# shunt blueprint: Zhipu (GLM China)

You are a coding agent adding Zhipu's China BigModel GLM endpoint to an operator's shunt gateway. Follow these steps; verify each before moving on.

## Prerequisites

- Confirm `shunt` is installed and `shunt --help` runs.
- Confirm the operator has a Zhipu GLM Coding Plan API key from BigModel.

## Locate the config

Honor an explicit `--config` path first. Otherwise, edit the active `shunt.toml`, `shunt.yaml`, or `shunt.yml`; if none exists, create `./shunt.toml`. Do not replace or weaken existing entries.

## Add the upstream

The `zhipu` preset supplies `kind = "anthropic"`, `base_url = "https://open.bigmodel.cn/api/anthropic"`, and API-key auth from `ZHIPUAI_API_KEY`:

```toml
[[upstreams]]
name = "zhipu"
provider = "zhipu"
```

When replacing the built-in provider set with `[[upstreams]]`, keep an `anthropic` passthrough upstream unless the operator intentionally changes `server.default_provider`.

## Credentials

Export the key in the environment that launches shunt:

```bash
export ZHIPUAI_API_KEY='...'
```

Never write, print, log, or commit the key.

## Optional model routing

```toml
[[models]]
id = "claude-glm-via-zhipu"
display_name = "GLM (via Zhipu)"

[models.upstream_model]
zhipu = "glm-5.3-flash"
```

Verify the model ID against the operator's Zhipu plan. Remove Claude Code's `[1m]` hint from `upstream_model`; shunt strips it from inbound requests before matching.

## Validate

Run `shunt check` in the same environment as the key and continue only after it prints `config ok`. Start `shunt run`, send one minimal `/v1/messages` request for a routed model, and confirm `x-gateway-upstream` is `zhipu`.

## Safety rules

- Keep credentials in the process environment or a secret manager.
- Preserve unrelated configuration and security controls.
- Make the smallest reversible edit and report exactly what changed.
