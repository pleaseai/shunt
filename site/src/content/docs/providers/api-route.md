---
title: API Route
description: Route models through API Route with your own API key.
---

[API Route](https://api-route.com) is an LLM gateway. This preset uses its Anthropic Messages endpoint at `/v1/messages`, with Bearer authentication from `API_ROUTE_API_KEY`. API Route usage is billed separately from Claude subscriptions.

## Configure

```bash
shunt add upstream api-route --print | claude
```

```toml
[[upstreams]]
name = "anthropic"
provider = "anthropic"

[[upstreams]]
name = "api-route"
provider = "api-route"

[[routes]]
model = "gpt-6.1-sol"
provider = "api-route"
```

Keep the Anthropic upstream as the default for unrouted models. Do not mix ordered upstreams with legacy provider tables.

## Credentials

```bash
export API_ROUTE_API_KEY='...'
```

Create your own key at [API Route](https://api-route.com). Keep it out of configuration files and source control. `shunt check` validates configuration structure; it does not validate credentials.

## Models and verification

Query the authenticated model list with your key. Availability depends on the account; verify Messages support before selecting another model.

```bash
curl -sS https://global.api-route.com/v1/models \
  -H "Authorization: Bearer $API_ROUTE_API_KEY"
```

```bash
shunt check
shunt run
curl -sSi http://127.0.0.1:3001/v1/messages \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-6.1-sol","max_tokens":32,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

Confirm `x-gateway-upstream: api-route` in the response. See [Connect Claude Code](/guides/connect-claude-code/) and [Model Discovery](/guides/model-discovery/) for client setup and model aliases.
