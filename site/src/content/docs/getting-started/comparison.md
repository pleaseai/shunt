---
title: Comparison
description: How shunt compares to other Claude Code gateways and LLM proxies — peer groups, feature matrix, strengths, and deliberate scope boundaries.
---

A grounded comparison of shunt against the tools it sits closest to. The goal is to make shunt's design boundaries explicit: what it deliberately does *not* do, and where the real, in-scope improvement opportunities are.

:::note[Scope]
Claims about shunt cite `file:line` in the [shunt repository](https://github.com/pleaseai/shunt). Claims about CLIProxyAPI were verified against `router-for-me/CLIProxyAPI@main`. Claims about general gateways (LiteLLM/Portkey/bifrost) are kept at the level the projects themselves advertise and match shunt's own README "Related work" framing.
:::

## 1. What shunt is (and isn't)

shunt is a **spec-compliant Claude Code LLM gateway**: it implements Claude Code's official `ANTHROPIC_BASE_URL` gateway contract (`/v1/messages`, `/v1/models` discovery, attribution/header pass-through) and does **selective, per-`model`-id** diversion — keep the main session on Claude, divert only the models you name onto another provider (ChatGPT/Codex, OpenAI, xAI). It translates Anthropic Messages ⇄ the OpenAI Responses API for mapped models, and passes everything else through to Anthropic unchanged. Routing is purely by the request's `model` id — no prompt-shape fingerprinting (`README.md:104-131`).

That focus is the axis every comparison below turns on. shunt optimizes for **translation fidelity and Claude-Code-native behavior**, with an Anthropic OAuth account pool that combines model-aware proactive quota rotation with reactive failover rather than broad multi-tenant fleet operation.

## 2. The peer groups

| Group | Examples | Relationship to shunt |
|---|---|---|
| **Subscription-backed CC proxy (same class)** | **raine/claude-code-proxy** | **Closest peer overall** — Rust single binary, per-`model` routing, Codex WebSocket + `previous_response_id` continuation, 4 subscription-OAuth backends (Codex/Kimi/Grok/Cursor) |
| **Broad Claude-Code proxy w/ Codex OAuth** | **CLIProxyAPI** (router-for-me) | Closest *broad* peer — overlaps on Codex/ChatGPT OAuth, Codex WebSocket v2, tool translation |
| **Narrow Claude Code → Codex swap** | insightflo/chatgpt-codex-proxy | Same inference-layer swap, single backend |
| **General Claude-Code routers** | musistudio/claude-code-router, 1rgs/claude-code-proxy, fuergaosi233/claude-code-proxy | Usually a *global* model swap, not per-model diversion |
| **General AI gateways** | LiteLLM, Portkey, bifrost, modelgate | Adjacent infra — possible *backends*, not Claude-Code-native |

## 3. Feature matrix

Legend: ● full · ◐ partial / workaround · ○ none · — n/a by design

| Capability | shunt | raine/ccp | CLIProxyAPI | Gen. CC routers | Gen. gateways |
|---|:--:|:--:|:--:|:--:|:--:|
| Claude Code gateway-protocol compliance (`/v1/models` discovery, attribution pass-through) | ● | ◐ | ◐ | ◐ | ○ |
| Selective per-`model`-id diversion, main session stays on Claude (not a global swap) | ● | ◐³ | ◐ | ○ | ◐ |
| Anthropic Messages ⇄ OpenAI Responses translation | ● | ● | ● | ◐ (mostly chat-completions) | ◐ (chat-completions) |
| ChatGPT/Codex **subscription** (OAuth) backend | ● | ●⁴ | ● | rare | ○ |
| Codex **WebSocket** Responses transport | ● | ● | ● | ○ | ○ |
| Upload trimming (`previous_response_id` continuation) **on the translation path** | ● | ● | ○ (passthrough only) | ○ | ○ |
| tool-search / `defer_loading` / `tool_reference` handling | ◐ (shim: works, no ctx savings; native opt-in⁸) | ○⁵ | ◐ (upstream) / ● (fork) | ○ | ○ |
| Reasoning round-trip to Claude Code `thinking` | ● (encrypted) | ◐ (Kimi/Grok; **Codex dropped**) | ◐ | ○ | ◐ |
| Multi-account load balancing / failover | ◐⁷ | ○ | ● | some | ● |
| Backend breadth | 4 providers¹ | 4 subs⁶ | 11 backends² | varies | 100–1600+ |
| Management API / dashboard | ◐ (opt-in admin surface) | ◐ (monitor TUI) | ● | some | ● |
| Usage / quota / cost tracking | ○ (Sentry metrics only) | ○ | ● | some | ● |
| Plugin / interceptor system | ○ | ○ | ● | some | ● |
| Language / footprint | Rust, 1 binary | Rust, 1 binary | Go | Node/Python | Go/Node/Python |
| Config model | TOML + env, hot-reload | env + config file | YAML + mgmt API | varies | YAML/UI |

¹ shunt: two adapter *kinds* (`anthropic` passthrough, `responses` translation) with 4 built-in providers (Anthropic, OpenAI, ChatGPT/Codex, xAI) — any Anthropic-Messages or OpenAI-Responses endpoint is config-only (`src/config.rs:180-190,316-363`).
² CLIProxyAPI: aistudio, antigravity, claude, codex, codex-ws, gemini, gemini-vertex, kimi, openai-compat, xai, xai-ws.
³ raine/ccp routes by `ANTHROPIC_MODEL` per-model like shunt, but has **no Anthropic-passthrough adapter** — an unknown model id returns 400, so you cannot keep the main session on Claude while diverting only the models you name.
⁴ raine/ccp implements its **own** ChatGPT OAuth (PKCE browser + device-code login); shunt reuses the Codex CLI login (`~/.codex/auth.json`) and its own PKCE flow is an open TODO (`src/auth/mod.rs:18-19`).
⁵ **Confirmed by reading raine/ccp source** (`fe80a6b`, 2026-07-11): no tool-search handling exists (zero matches for `defer_loading` / `tool_reference` / `tool_search` / `advanced-tool-use`). Tools are whitelist-rebuilt to `{name, description, parameters}` (`src/providers/codex/translate/request.rs:476-494`), so `defer_loading:true` is silently dropped — no 400, but no context saved; a `tool_reference` block in a ToolSearch result renders as `[unsupported content block omitted: tool_reference]` (`request.rs:836-842`) rather than shunt's clean `"Loaded tool: X"`. Hence ○ (vs shunt's ◐): force-enabling `ENABLE_TOOL_SEARCH` against raine/ccp degrades the discovery-loop result to a placeholder. By default Claude Code's own gate keeps tool search off behind a non-first-party base URL, so this stays latent.
⁶ raine/ccp subscription backends: Codex (ChatGPT Plus/Pro), Kimi (kimi.com), Grok (grok.com), Cursor Agent — all via subscription OAuth.
⁷ shunt pools explicit accounts only for Anthropic `claude_oauth`: session-sticky selection, per-provider round-robin, model-aware proactive rotation from per-account 5h/7d quota headers, cooldowns, forced refresh after 401, and reactive failover on quota-rejected 429s and 5xx responses. ChatGPT/Codex remains single-account; per-account usage reporting is not implemented.
⁸ **[#82]** adds an opt-in, per-provider `tool_search` flag (`src/config.rs:250-261,1041-1049`) that maps Claude Code's tool search onto the OpenAI Responses API's own native, client-executed `tool_search` protocol — `ToolSearch` → `tool_search`, its `tool_use` → `tool_search_call`, and `tool_reference` → a `tool_search_output` item carrying the loaded tools' full schemas as structured JSON (`src/model/responses_request.rs`) — instead of folding schema into text. Off by default: it only applies for a stock OpenAI or ChatGPT/Codex Responses flavor routing to a gpt-5.4+ model, and is gated behind the flag until a live probe confirms a given backend accepts the shapes shunt emits. xAI/Grok routes and gpt-5.2-and-below models keep the #43 shim regardless of the flag.

> "raine/ccp" = [raine/claude-code-proxy](https://github.com/raine/claude-code-proxy).

## 4. Where shunt leads

- **Claude-Code-native fidelity.** shunt implements the *official* gateway contract instead of the "hash the subagent system prompt" heuristic older CC proxies use; the session stays inside Claude Code's harness (same tool loop, skills, script paths) — only token generation is outsourced (`README.md:97-131`). Most general routers and gateways are OpenAI-chat-completions-centric and don't honor Claude Code's discovery/attribution surface.

- **Upload trimming on the *translation* path.** Because shunt translates Anthropic ⇄ Responses (Claude Code never sends `previous_response_id`), it *synthesizes* continuation: it stores the transcript on the pooled connection, diffs the next request against it with type-aware normalization, and injects `previous_response_id` + input-delta — real upload trimming on the Claude→Codex path (`src/adapters/codex_continuation.rs:79-114`). This is **not** unique: **raine/claude-code-proxy does the same class of thing** (opt-in `CCP_CODEX_PREVIOUS_RESPONSE_ID`, session-keyed, append-only). The two Rust subscription proxies share it — the real contrast is with **passthrough** proxies like **CLIProxyAPI**, whose Codex WS stores no transcript/response-id, relies on the Codex CLI client to send `previous_response_id`, and therefore re-sends full input every turn on *its* translation path (plus a tool-output "repair" cache to keep tool-call pairing consistent).

- **Normalization depth + reasoning fidelity (vs the nearest peer).** Within that shared-continuation pair, shunt goes further than raine/claude-code-proxy on two axes: (1) its continuation normalization parses `function_call.arguments` and round-trips reasoning `encrypted_content`/signature, so continuation keeps firing across tool turns where a shape-only comparison would drop it (`src/adapters/codex_continuation.rs:11-48`); and (2) it **forwards Codex reasoning to Claude Code as `thinking`**, whereas raine/claude-code-proxy **drops Codex reasoning blocks entirely** (its README lists this as a limitation). Any unforeseen shape still falls back to full input — never wrong context, only a missed optimization.

- **Small, auditable footprint.** Single Rust binary, TOML+env config with fail-closed boot validation and hot-reload; no runtime plugin surface to secure.

## 5. Where shunt trails — and why

Most gaps are **deliberate scope boundaries**, not oversights. shunt's own README positions general gateways (LiteLLM/Portkey/bifrost) as *adjacent infrastructure / possible backends*, not the same product.

- **Anthropic OAuth multi-account is deliberately narrow.** shunt has a proactive and reactive account pool for `auth = "claude_oauth"`: `x-claude-code-session-id` stickiness, per-provider round-robin, model-aware rotation before the 5h or governing weekly bucket reaches the wall, account cooldowns, credentials-file force-refresh after 401, and failover after quota-rejected 429s or 5xx responses ([details](/guides/anthropic-multi-account/)). It does **not** pool ChatGPT/Codex accounts, ramp concurrency on a freshly switched account, or expose per-account usage. CLIProxyAPI, LiteLLM, and Portkey provide broader fleet-oriented balancing and visibility; see §6, items G–H for the remaining gap.
- **Narrow backend breadth.** Only Anthropic-Messages passthrough or OpenAI-Responses translation; no native Gemini/Bedrock/Azure/Ollama unless they expose one of those two protocols.
- **No full management API / usage-quota / cost tracking.** The opt-in [admin web surface](/guides/admin-remote-provisioning/) covers browser account provisioning and read-only account-pool health for `claude_oauth` providers, but there is no general management API, per-request usage accounting, or cost tracking; observability is opt-in Sentry metrics only (`src/metrics.rs`). The full HTTP surface is listed in [HTTP Endpoints](/reference/endpoints/). CLIProxyAPI ships a full management API + quota/usage manager and a third-party dashboard ecosystem; even the same-class peer raine/claude-code-proxy ships a built-in **monitor TUI** (live sessions, active / recent requests, error events) that shunt has no equivalent of.
- **No own ChatGPT OAuth login.** shunt reuses the Codex CLI login (`~/.codex/auth.json`); a first-party PKCE flow is an open TODO (`src/auth/mod.rs:18-19`). raine/claude-code-proxy is prior art here — it ships its own `codex auth login` (PKCE) **and** `codex auth device` (device-code), so it works without the Codex CLI installed.
- **No plugin / interceptor system.** The adapter set is a fixed two-variant `match` (`src/proxy.rs:152-163`); CLIProxyAPI has a full plugin host (RPC ABI, auth providers, executor routing, request/response translators).
- **Plain HTTP only** (TLS out of scope, `docs/m4-inbound-auth.md:13`).

## 6. Improvement opportunities (from this comparison)

Ordered by fit with shunt's mission. **In-scope** items advance high-fidelity translation / Claude-Code-native behavior; **scope-boundary** items would move shunt toward being a fleet gateway and warrant a conscious decision first.

### In-scope

- **A. tool-search context savings (already tracked: [#43]).** shunt renders `tool_reference` as name-only `"Loaded tool: X"` text and forwards *all* deferred tool schemas upfront (`src/model/responses_request.rs:393-403,475-508`) — the loop works but reclaims zero context by default. Port the server-side emulation (filter deferred+unloaded tools, inject full schema on `tool_reference`) — reference implementation: CLIProxyAPI PR #1892 (`Adamcf123/CLIProxyAPI@main`). **Partially addressed by [#82]**: an opt-in `tool_search = true` per-provider flag now maps tool search onto the Responses API's native, client-executed `tool_search` protocol instead of the text shim, for a stock OpenAI or ChatGPT/Codex provider routing to a gpt-5.4+ model (see footnote 8 above). It's off by default pending a live probe of backend acceptance, so the shim (and the zero-savings gap for xAI/Grok and older models) remains the baseline until operators opt in.

- **B. Codex WS: live-probe the continuation normalization (already tracked: [#45]).** Reasoning/`function_call` normalization is schema-validated against 3 sources but not yet live-probed (`docs/m7-codex-websocket.md:250-270`). Any unaccounted field silently falls back to the safe full-input fallback — correctness-safe, but a *latent missed optimization*. A probe pass would confirm continuation fires as often as it should.

- **C. Codex WS: mid-stream failure resumption (already tracked: [#46]).** A WS failure *before* streaming falls back to HTTP transparently, but a *mid-stream* failure surfaces as an error SSE event, not a fallback (`src/adapters/responses.rs:92-135`). Consider resuming/replaying so a dropped socket mid-turn degrades to HTTP instead of erroring.

- **D. Codex WS: speculative prewarm (`generate:false`) (already tracked: [#47]).** Explicitly out of scope today (`docs/m7-codex-websocket.md:53-58`), but it is a real Codex latency optimization — prewarming the socket/context before the first token. Worth revisiting once continuation is live-probed.

- **E. Upstream retry/backoff (already tracked: [#48]).** The M4-planned bounded retry/backoff is not implemented (`docs/implementation-plan.md:247`); transient upstream 429/5xx errors surface directly. A small, idempotent retry would improve resilience without adding scope.

- **F. Doc drift: `GET /protocol` (already tracked: [#49]).** README advertises a machine-readable spec at `GET /protocol` (`README.md:110`) but no such route exists in `src/server.rs`. Implement it (cheap, and it's part of the gateway-protocol story) or correct the docs.

### Scope-boundary (decide before doing)

- **G. Minimal multi-account for ChatGPT/Codex.** Full LB is out of scope, but heavy users hit ChatGPT/Codex rolling-window caps, where a *fill-first* rotation across a handful of `~/.codex/auth.json`-style logins (burn one account's window before moving to the next) is disproportionately valuable. This is the single biggest feature gap vs CLIProxyAPI and the one most worth a design discussion.

- **H. Per-account quota/usage visibility.** Follows G — if multiple subscription accounts are in play, surfacing each account's 5h/7d window (as CLIProxyAPI's ecosystem does) becomes useful. Ties to the observability gap.

- **I. Native Gemini (and other) backends.** Only relevant if shunt broadens past the Anthropic-Messages / OpenAI-Responses duality. Not currently in scope.

## 7. One-line takeaway

shunt is the **high-fidelity, Claude-Code-native** end of the spectrum. Its nearest peer is **raine/claude-code-proxy** — same class (Rust, subscription OAuth, per-`model` routing, Codex WS + `previous_response_id` continuation) — against which shunt's edge is deeper continuation normalization, Codex reasoning fidelity (raine drops it), an Anthropic-passthrough path (keep the main session on Claude), and xAI OAuth; raine's edge is a built-in monitor TUI, a first-party ChatGPT OAuth login, and Kimi/Cursor breadth. Against **CLIProxyAPI**, shunt wins on translation-path upload trimming (CLIProxyAPI's WS is a passthrough) and trades away most fleet features (broad multi-account LB, full management APIs, plugins, backend breadth) by design. It now provides a narrow Anthropic OAuth account pool with model-aware proactive quota scheduling plus reactive failover, but ChatGPT/Codex pooling remains a deliberate gap. The highest-value in-scope work is finishing the tool-search context savings ([#43]) — now partly addressed by an opt-in native `tool_search` path on Codex/OpenAI ([#82]) — and hardening the Codex WS continuation (live-probe + mid-stream fallback); the biggest deliberate gap to weigh is minimal fill-first multi-account for ChatGPT/Codex.

[#43]: https://github.com/pleaseai/shunt/issues/43
[#82]: https://github.com/pleaseai/shunt/issues/82
[#45]: https://github.com/pleaseai/shunt/issues/45
[#46]: https://github.com/pleaseai/shunt/issues/46
[#47]: https://github.com/pleaseai/shunt/issues/47
[#48]: https://github.com/pleaseai/shunt/issues/48
[#49]: https://github.com/pleaseai/shunt/issues/49
