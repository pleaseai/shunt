# shunt-xai

Claude Code subagents that run on xAI's **Grok** models — **grok-build-0.1**,
**grok-4.5**, and **grok-4.3** — routed through the
[shunt](https://github.com/pleaseai/shunt) gateway.

> **EXPERIMENTAL.** shunt's `xai` provider is not yet verified against the live
> xAI API (see [`docs/m6-xai-provider.md`](https://github.com/pleaseai/shunt/blob/main/docs/m6-xai-provider.md)).
> These agents may need adjustment as the integration is hardened.

Unlike a CLI hand-off (which drops persona and preloaded skills), shunt diverts
only *token generation* at the inference layer. The session keeps running inside
Claude Code's harness: same tool loop, same skills, same script-path resolution.
Only the model that generates the tokens changes.

## Agents

| Agent (`@`-mention)          | Model id (`model:`) | Notes                        |
| ---------------------------- | ------------------- | ---------------------------- |
| `shunt-xai:grok-build-0.1`   | `grok-build-0.1`    | Flagship Grok coding model   |
| `shunt-xai:grok-4.5`         | `grok-4.5`          |                              |
| `shunt-xai:grok-4.3`         | `grok-4.3`          |                              |

Each agent's `model:` frontmatter pins the request to a Grok slug, so only that
subagent diverts — the main session stays on Claude.

> **Reasoning effort is opt-in.** Several Grok models (`grok-4*`, `grok-3`,
> `grok-code-fast`, …) return `400` on `reasoning.effort` even though they reason
> natively. shunt therefore only sends the effort dial when you configure an
> `effort` on the provider or route (or pass one per request). Leave it unset to
> use each model's native reasoning. Model slugs are reference-only — the
> catalog is xAI's; shunt passes whatever slug you route through.

## Prerequisites

These agents only work when a shunt gateway is running in front of Claude Code
and is configured to route the Grok slugs above to the `xai` provider (which is a
built-in provider — you only need a credential and a route).

1. **Run shunt** and point Claude Code at it:
   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:3001   # shunt's default bind address
   ```
2. **Provide an xAI credential** — either an API key or a reused
   SuperGrok / X Premium+ subscription:
   ```bash
   export XAI_API_KEY=…            # API-key path (built-in default)
   # or, to reuse a subscription: set auth = "xai_oauth" and run `shunt login xai`
   ```
3. **Map the slugs** in your `shunt.toml` to the `xai` provider:
   ```toml
   [[routes]]
   model = "grok-build-0.1"
   provider = "xai"

   [[routes]]
   model = "grok-4.5"
   provider = "xai"

   [[routes]]
   model = "grok-4.3"
   provider = "xai"
   ```

   For the full setup — API-key vs. subscription OAuth, the reasoning gate, and
   troubleshooting — see [`docs/m6-xai-provider.md`](https://github.com/pleaseai/shunt/blob/main/docs/m6-xai-provider.md).

Without a running shunt gateway mapping these ids, Claude Code will send the
`grok-*` model id straight to Anthropic and the request will fail.

## Install

```
/plugin marketplace add pleaseai/shunt
/plugin install shunt-xai@shunt
```

## Usage

```
@shunt-xai:grok-build-0.1  refactor this module and run the tests
```

Or set every subagent to a Grok model for a session with
`CLAUDE_CODE_SUBAGENT_MODEL=grok-4.5`.

Both require a running shunt gateway with the slug routed to the `xai` provider —
see [Prerequisites](#prerequisites). Without it the request fails against Anthropic.

## License

MIT OR Apache-2.0, matching the shunt project.
