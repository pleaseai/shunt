---
title: Troubleshooting
description: Common shunt errors and how to fix them.
---

| Symptom | Cause / Fix |
| :-- | :-- |
| `ChatGPT auth not found; run codex login` | shunt can't read `~/.codex/auth.json`. Run `codex login`. |
| `authentication_error` on a mapped model | Expired/absent provider credential — re-run `codex login`, or export `OPENAI_API_KEY`. shunt surfaces the backend's real `detail` message. |
| `400 … model is not supported when using Codex with a ChatGPT account` | You used a `-codex` slug (or one your account isn't entitled to). Use an entitled slug from [models.json](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) (e.g. `gpt-5.6-sol`, `gpt-5.5`) or set `upstream_model`. |
| `/model` doesn't list your model | For `gpt-*` ids use `ANTHROPIC_CUSTOM_MODEL_OPTION`; [discovery](/guides/model-discovery/) only surfaces `claude`/`anthropic`-prefixed ids. |
| Discovery never fires | It's gated on a gateway credential (`ANTHROPIC_AUTH_TOKEN`, API key, or `apiKeyHelper`) plus `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`. Debug with `claude --debug` → `[gatewayDiscovery]` lines. |
| `config check failed` | Run `shunt check` for the exact reason (bind address, unknown provider in a route, wrong adapter/auth). |
| Claude Code asks you to log in | Set an Anthropic credential (`ANTHROPIC_AUTH_TOKEN` / login) that shunt can forward for unmapped models. A base URL alone is not a credential. |
| Effort stuck at `medium` on a mapped model | Set `CLAUDE_CODE_ALWAYS_ENABLE_EFFORT=1` — see [Effort & Context](/guides/effort-and-context/#reasoning-effort). |
| Tool search inactive on a mapped model (every tool's schema sent each turn) | Set `ENABLE_TOOL_SEARCH=true`. Claude Code auto-disables optimistic tool search behind a non-Anthropic base URL; shunt forwards `tool_reference` blocks and reveals deferred schemas on demand — see [ChatGPT / Codex → Tool search](/guides/codex/#tool-search). |
| Session stuck after a context-length error on a mapped model | shunt rewrites upstream overflow errors to `prompt is too long …` so Claude Code auto-compacts and retries — see [Context overflow recovery](/guides/effort-and-context/#context-overflow-recovery). If it recurs every few turns, lower `CLAUDE_CODE_MAX_CONTEXT_TOKENS` to the model's real window. |
| Stream dies behind Cloudflare (524) | Keep [`sse_keepalive_seconds`](/guides/shared-gateway/#sse-keepalive-pings) at its default (30) instead of `0`. |
| 401 on mapped models on a shared gateway | Missing/invalid client token — set `ANTHROPIC_CUSTOM_HEADERS="x-shunt-token: <token>"`; see [Sharing a Gateway](/guides/shared-gateway/). |

For the full gateway troubleshooting table, see [Connect Claude Code to an LLM gateway](https://code.claude.com/docs/en/llm-gateway-connect#troubleshoot-gateway-errors).
