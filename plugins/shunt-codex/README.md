# shunt-codex

Claude Code subagents that run on ChatGPT/Codex **GPT-5.6** models — **Luna**,
**Sol**, and **Terra** — routed through the [shunt](https://github.com/pleaseai/shunt)
gateway.

Unlike a CLI hand-off (which drops persona and preloaded skills), shunt diverts
only *token generation* at the inference layer. The session keeps running inside
Claude Code's harness: same tool loop, same skills, same script-path resolution.
Only the model that generates the tokens changes.

## Agents

| Agent (`@`-mention)         | Model id (`model:`) | Native effort | Supported effort levels                     |
| --------------------------- | ------------------- | ------------- | ------------------------------------------- |
| `shunt-codex:gpt-5.6-sol`   | `gpt-5.6-sol`       | low (fast)    | low · medium · high · xhigh · max · ultra   |
| `shunt-codex:gpt-5.6-terra` | `gpt-5.6-terra`     | medium        | low · medium · high · xhigh · max · ultra   |
| `shunt-codex:gpt-5.6-luna`  | `gpt-5.6-luna`      | medium        | low · medium · high · xhigh · max           |

Each agent's `model:` frontmatter pins the request to a Codex slug, so only that
subagent diverts — the main session stays on Claude. All three share a 372k-token
context window.

> **Effort levels are from openai/codex's [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json).**
> Note the difference: **Luna does not support the `ultra` level** — its top level
> is `max`. Sending `ultra` to Luna (via shunt's `effort` config or an `effort:`
> override) is rejected upstream. Sol and Terra accept `ultra`.

> The agents' system prompts are written for **Claude Code's harness** — these
> models run inside Claude Code's tool loop (Read/Edit/Bash, skills), not Codex's.
> The `gpt-5.6-*` entries in `models.json` ship no `instructions_template`, so
> there is nothing Codex-specific to inherit; shunt only diverts token generation.

## Prerequisites

These agents only work when a shunt gateway is running in front of Claude Code
and is configured to route the model ids above to the Codex provider:

1. **Run shunt** and point Claude Code at it:
   ```bash
   export ANTHROPIC_BASE_URL=http://localhost:8787   # your shunt address
   ```
2. **Authenticate the ChatGPT/Codex subscription** shunt reuses:
   ```bash
   codex login   # writes ~/.codex/auth.json, which shunt reads + auto-refreshes
   ```
3. **Map the slugs** in your `shunt.toml` to the Codex provider (see
   [`shunt.toml.example`](https://github.com/pleaseai/shunt/blob/main/shunt.toml.example)):
   ```toml
   [[routes]]
   model = "gpt-5.6-sol"     # (and gpt-5.6-luna / gpt-5.6-terra)
   provider = "codex"
   ```

> The ChatGPT-account backend only accepts the slugs your account is entitled to.
> The latest are `gpt-5.6-sol` / `gpt-5.6-terra` / `gpt-5.6-luna`; older accounts
> may only have `gpt-5.5` / `gpt-5.4` / `gpt-5.2`. The canonical catalog is
> openai/codex's [`models.json`](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json).

Without a running shunt gateway mapping these ids, Claude Code will send the
`gpt-5.6-*` model id straight to Anthropic and the request will fail.

## Install

```
/plugin marketplace add pleaseai/shunt
/plugin install shunt-codex@shunt
```

## Usage

```
@shunt-codex:gpt-5.6-sol  refactor this module and run the tests
```

Or set every subagent to a Codex model for a session with
`CLAUDE_CODE_SUBAGENT_MODEL=gpt-5.6-sol`.

## License

MIT OR Apache-2.0, matching the shunt project.
