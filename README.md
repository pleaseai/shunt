# shunt

> Shunt Claude Code agents to any model.

`shunt` is a transparent proxy that routes **specific Claude Code agents/subagents** to different LLM providers or runtimes at the **inference layer**. It diverts only the selected traffic (the "shunt") — everything else passes through to Anthropic unchanged.

The name is the mechanism: an electrical/railway *shunt* diverts a selected part of the flow onto a parallel path. Here, a mapped agent's inference is diverted to another model while Claude Code's tools, skills, and `${CLAUDE_SKILL_DIR}` resolution stay intact.

**Status:** private, early. May be open-sourced later.

## Why

Claude Code sends every agent turn to the Anthropic API. `shunt` sits in front (via `ANTHROPIC_BASE_URL`) and, for the agents you map, diverts their inference to another provider (OpenAI, etc.) or runtime. Because routing happens at the HTTP/inference layer — not by handing the task off to a different CLI — the agent keeps running inside Claude Code's harness: same tool loop, same preloaded skills, same bundled-script path resolution. Only token generation is outsourced.

Contrast with the alternative approach (handing a `subagent_type` off to another runtime like Codex CLI), which cuts higher in the stack and drops persona, preloaded skills, and breaks `${CLAUDE_SKILL_DIR}` script references.

## Related work / prior art

**Claude Code–specific routers & proxies**

- [musistudio/claude-code-router](https://github.com/musistudio/claude-code-router) — the largest in this niche; use Claude Code as a foundation and decide how requests reach different models/providers.
- [1rgs/claude-code-proxy](https://github.com/1rgs/claude-code-proxy) — run Claude Code on OpenAI models.
- [fuergaosi233/claude-code-proxy](https://github.com/fuergaosi233/claude-code-proxy) — Claude Code → OpenAI API proxy.
- [seifghazi/claude-code-proxy](https://github.com/seifghazi/claude-code-proxy) — captures/visualizes in-flight Claude Code requests, with optional **per-agent** routing to other providers (the direct inspiration for `shunt`'s subagent-routing idea).
- [luohy15/y-router](https://github.com/luohy15/y-router) — a simple proxy enabling Claude Code to work with OpenRouter.
- [tingxifa/claude_proxy](https://github.com/tingxifa/claude_proxy) — Cloudflare Workers proxy translating Claude API requests to OpenAI format (Gemini, Groq, Ollama).
- [badlogic/claude-bridge](https://github.com/badlogic/claude-bridge) — use any model provider with Claude Code.
- [jimmc414/claude_n_codex_api_proxy](https://github.com/jimmc414/claude_n_codex_api_proxy) — cross-runtime router: proxies Anthropic **or** OpenAI API calls to the local **Claude Code or Codex** CLI (routes to the local CLI when the API key is all 9s, else the real cloud API). Note the inverse direction — routing cloud-API calls *to* local CLIs, rather than routing Claude Code agents *out* to cloud providers.
- [insightflo/chatgpt-codex-proxy](https://github.com/insightflo/chatgpt-codex-proxy) — Anthropic-compatible `/v1/messages` proxy that serves Claude Code inference from the **ChatGPT Codex backend** (uses a ChatGPT Plus/Pro subscription instead of an API key). Same inference-layer swap as `shunt`, targeting the Codex/GPT subscription backend while keeping Claude Code's UI and MCP tools.

**General AI gateways (adjacent infrastructure — possible backends)**

- [BerriAI/litellm](https://github.com/BerriAI/litellm) — SDK + proxy/AI gateway calling 100+ LLM APIs in OpenAI format, with cost tracking, guardrails, load balancing.
- [Portkey-AI/gateway](https://github.com/Portkey-AI/gateway) — fast AI gateway routing to 1,600+ LLMs with integrated guardrails.
- [maximhq/bifrost](https://github.com/maximhq/bifrost) — high-performance AI gateway with adaptive load balancing and 1000+ model support.

### How `shunt` differs

Most Claude Code proxies above route **all** traffic to one alternative provider (a global model swap). `shunt`'s focus is **selective, per-agent** diversion: keep the main session on Claude, and shunt only the agents you name onto other models — the switchboard/patchbay use case, applied at the agent granularity.

## Claude Code integration (official surface)

Claude Code exposes a **first-class gateway contract** behind `ANTHROPIC_BASE_URL` — `shunt` should implement this rather than the fragile "hash the subagent's system prompt" heuristic that earlier Claude Code proxies rely on.

- [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol) — the API contract: endpoints, headers/body fields to forward vs consume, feature pass-through, and attribution. A running gateway serves the machine-readable spec at `GET /protocol`.
  - [Model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery) — Claude Code queries `GET /v1/models?limit=1000` at startup (opt-in via `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) and adds returned models to the `/model` picker. **Constraint:** entries whose `id` doesn't begin with `claude`/`anthropic` are ignored — non-Claude models must be aliased or added manually.
  - **System prompt attribution block** — Claude Code prepends a client-version + conversation fingerprint to the system prompt; stable for the conversation lifetime (v2.1.181+). A cleaner routing/caching signal than hashing the full prompt.
- [Add a custom model option](https://code.claude.com/docs/en/model-config#add-a-custom-model-option) — `ANTHROPIC_CUSTOM_MODEL_OPTION` adds a gateway-routed entry to the `/model` picker without replacing built-in aliases; the ID skips validation, so any string the gateway accepts works.

**Design implication for `shunt`:** be a spec-compliant Anthropic-Messages gateway (`/v1/messages`, `/v1/models`, `/protocol`, correct header/attribution pass-through), and drive per-agent diversion from stable protocol signals rather than prompt-shape heuristics that break on every Claude Code prompt change.

## License

TBD (private for now).
