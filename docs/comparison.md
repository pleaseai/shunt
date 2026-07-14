# shunt vs. other Claude Code gateways & LLM proxies

A grounded comparison of `shunt` against the tools it sits closest to. The goal is
to make shunt's design boundaries explicit: what it deliberately does *not* do, and
where the real, in-scope improvement opportunities are.

> Scope note: claims about shunt cite `file:line` in this repo. Claims about
> CLIProxyAPI were verified against `router-for-me/CLIProxyAPI@main`. Claims about
> general gateways (LiteLLM/Portkey/bifrost) are kept at the level the projects
> themselves advertise and match shunt's own README "Related work" framing.

## 1. What shunt is (and isn't)

shunt is a **spec-compliant Claude Code LLM gateway**: it implements Claude Code's
official `ANTHROPIC_BASE_URL` gateway contract (`/v1/messages`, `/v1/models`
discovery, attribution/header pass-through) and does **selective, per-`model`-id**
diversion — keep the main session on Claude, divert only the models you name onto
another provider (ChatGPT/Codex, OpenAI, Cursor, xAI, Grok). It translates Anthropic Messages ⇄
the OpenAI Responses API for mapped models, and passes everything else through to
Anthropic unchanged. Routing is purely by the request's `model` id — no
prompt-shape fingerprinting (`README.md:104-131`).

That focus is the axis every comparison below turns on. shunt optimizes for
**translation fidelity and Claude-Code-native behavior**, with an Anthropic
OAuth account pool that combines model-aware proactive quota rotation with reactive
failover rather than broad multi-tenant fleet operation.

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
| Backend breadth | 6 providers¹ | 4 subs⁶ | 11 backends² | varies | 100–1600+ |
| Management API / dashboard | ◐⁹ | ◐ (monitor TUI) | ● | some | ● |
| Usage / quota / cost tracking | ◐⁹ (pool quota state; no usage/cost) | ○ | ● | some | ● |
| Plugin / interceptor system | ○ | ○ | ● | some | ● |
| Language / footprint | Rust, 1 binary | Rust, 1 binary | Go | Node/Python | Go/Node/Python |
| Config model | TOML + env, hot-reload | env + config file | YAML + mgmt API | varies | YAML/UI |

¹ shunt: three adapter *kinds* (`anthropic` passthrough, `responses` translation, and
`cursor` ConnectRPC/protobuf translation — `src/config.rs:395-408`) with 6 built-in providers
seeded by default (Anthropic, OpenAI, ChatGPT/Codex, Cursor, and xAI Grok on two surfaces —
`xai` API-key and `grok` subscription-OAuth — `src/config.rs:652-716`); any other
Anthropic-Messages or OpenAI-Responses endpoint is config-only.
² CLIProxyAPI: aistudio, antigravity, claude, codex, codex-ws, gemini, gemini-vertex,
kimi, openai-compat, xai, xai-ws.
³ raine/ccp routes by `ANTHROPIC_MODEL` per-model like shunt, but has **no
Anthropic-passthrough adapter** — an unknown model id returns 400, so you cannot keep
the main session on Claude while diverting only the models you name.
⁴ raine/ccp implements its **own** ChatGPT OAuth (PKCE browser + device-code login);
shunt reuses the Codex CLI login (`~/.codex/auth.json`) and its own PKCE flow is an
open TODO (`src/auth/mod.rs:19-20`).
⁵ **Confirmed by reading raine/ccp source** (`fe80a6b`, 2026-07-11): no tool-search
handling exists (zero matches for `defer_loading` / `tool_reference` / `tool_search` /
`advanced-tool-use`). Tools are whitelist-rebuilt to `{name, description, parameters}`
(`src/providers/codex/translate/request.rs:476-494`), so `defer_loading:true` is
silently dropped — no 400, but no context saved; a `tool_reference` block in a
ToolSearch result renders as `[unsupported content block omitted: tool_reference]`
(`request.rs:836-842`) rather than shunt's clean `"Loaded tool: X"`. Hence ○ (vs
shunt's ◐): force-enabling `ENABLE_TOOL_SEARCH` against raine/ccp degrades the
discovery-loop result to a placeholder. By default Claude Code's own gate keeps tool
search off behind a non-first-party base URL, so this stays latent.
⁶ raine/ccp subscription backends: Codex (ChatGPT Plus/Pro), Kimi (kimi.com), Grok
(grok.com), Cursor Agent — all via subscription OAuth.
⁷ shunt pools explicit accounts for both Anthropic `claude_oauth` and ChatGPT/Codex
`chatgpt_oauth`. The Anthropic pool is proactive **and** reactive: session-sticky
selection, per-provider round-robin, model-aware proactive rotation from per-account
5h/7d quota headers, cooldowns, forced refresh after 401, and reactive failover on
quota-rejected 429s and 5xx responses (`docs/m8-anthropic-multi-account.md`). The
Codex/ChatGPT pool (`docs/m10-codex-multi-account.md`) mirrors the reactive half only —
session-sticky/round-robin selection, cooldowns, forced refresh after 401, rotation on
429/5xx/credential-resolution failure — because the backend sends no per-account quota
headers, so there is no proactive near-quota switch and no fill-first ordering across
accounts. Per-account usage reporting is not implemented for either pool.
⁸ **[#82]** adds an opt-in, per-provider `tool_search` flag (`src/config.rs:326-337,1198-1211`)
that maps Claude Code's tool search onto the OpenAI Responses API's own native,
client-executed `tool_search` protocol — `ToolSearch` → `tool_search`, its `tool_use` →
`tool_search_call`, and `tool_reference` → a `tool_search_output` item carrying the loaded
tools' full schemas as structured JSON (`src/model/responses_request.rs`) — instead of folding
schema into text. Off by default: it only applies for a stock OpenAI or ChatGPT/Codex Responses
flavor routing to a gpt-5.4+ model, and is gated behind the flag until a live probe confirms a
given backend accepts the shapes shunt emits. xAI/Grok routes and gpt-5.2-and-below models keep
the #43 shim regardless of the flag.

⁹ **[#77]** adds an opt-in `[server.admin]` browser surface, registered only when the
`[server.admin]` table is present (`src/server.rs:117-118`, `src/admin/mod.rs:87-103`). It
provisions Anthropic `claude_oauth` accounts (add/list/replace/remove) through the existing
`claude_login` flow and renders a **read-only account-pool dashboard** — per-account 5h/7d
quota utilization, cooldown, and near-quota flags (`src/accounts.rs:46-63`).
Deliberately narrow: Anthropic accounts only (no ChatGPT/Codex), and no request/token usage
or cost accounting — well short of CLIProxyAPI's full management API + quota/usage manager.

> "raine/ccp" = [raine/claude-code-proxy](https://github.com/raine/claude-code-proxy).

## 4. Where shunt leads

- **Claude-Code-native fidelity.** shunt implements the *official* gateway contract
  instead of the "hash the subagent system prompt" heuristic older CC proxies use;
  the session stays inside Claude Code's harness (same tool loop, skills, script
  paths) — only token generation is outsourced (`README.md:97-131`). Most general
  routers and gateways are OpenAI-chat-completions-centric and don't honor Claude
  Code's discovery/attribution surface.

- **Upload trimming on the *translation* path.** Because shunt translates
  Anthropic ⇄ Responses (Claude Code never sends `previous_response_id`), it
  *synthesizes* continuation: it stores the transcript on the pooled connection,
  diffs the next request against it with type-aware normalization, and injects
  `previous_response_id` + input-delta — real upload trimming on the Claude→Codex
  path (`src/adapters/responses/codex_continuation.rs:79-114`). This is **not** unique:
  **raine/claude-code-proxy does the same class of thing** (opt-in
  `CCP_CODEX_PREVIOUS_RESPONSE_ID`, session-keyed, append-only). The two Rust
  subscription proxies share it — the real contrast is with **passthrough** proxies
  like **CLIProxyAPI**, whose Codex WS stores no transcript/response-id, relies on
  the Codex CLI client to send `previous_response_id`, and therefore re-sends full
  input every turn on *its* translation path (plus a tool-output "repair" cache to
  keep tool-call pairing consistent).

- **Normalization depth + reasoning fidelity (vs the nearest peer).** Within that
  shared-continuation pair, shunt goes further than raine/claude-code-proxy on two
  axes: (1) its continuation normalization parses `function_call.arguments` and
  round-trips reasoning `encrypted_content`/signature, so continuation keeps firing
  across tool turns where a shape-only comparison would drop it
  (`src/adapters/responses/codex_continuation.rs:11-48`); and (2) it **forwards Codex reasoning
  to Claude Code as `thinking`**, whereas raine/claude-code-proxy **drops Codex
  reasoning blocks entirely** (its README lists this as a limitation). Any unforeseen
  shape still falls back to full input — never wrong context, only a missed
  optimization.

- **Small, auditable footprint.** Single Rust binary, TOML+env config with
  fail-closed boot validation and hot-reload; no runtime plugin surface to secure.

## 5. Where shunt trails — and why

Most gaps are **deliberate scope boundaries**, not oversights. shunt's own README
positions general gateways (LiteLLM/Portkey/bifrost) as *adjacent infrastructure /
possible backends*, not the same product.

- **Multi-account pooling is deliberately narrow.** shunt has a proactive and reactive
  account pool for `auth = "claude_oauth"`: `x-claude-code-session-id` stickiness,
  per-provider round-robin, model-aware rotation before the 5h or governing weekly
  bucket reaches the wall, account cooldowns, credentials-file force-refresh after
  401, and failover after quota-rejected 429s or 5xx responses
  (`docs/m8-anthropic-multi-account.md`). ChatGPT/Codex (`auth = "chatgpt_oauth"`) now
  has a mirrored pool too, but **reactive-only** — session-sticky/round-robin
  selection, cooldowns, and failover on 401/429/5xx/credential-resolution failure,
  with no quota-aware proactive rotation because the backend exposes no per-account
  rate-limit headers (`docs/m10-codex-multi-account.md`). Neither pool ramps
  concurrency on a freshly switched account or exposes per-account usage.
  CLIProxyAPI, LiteLLM, and Portkey provide broader fleet-oriented balancing and
  visibility, and CLIProxyAPI in particular supports quota-aware fill-first ordering
  that shunt's Codex pool still lacks; see §6, item H for the remaining gap.
- **Narrow backend breadth.** Only Anthropic-Messages passthrough or OpenAI-Responses
  translation; no native Gemini/Bedrock/Azure/Ollama unless they expose one of those
  two protocols.
- **Narrow, opt-in management surface; no usage/cost tracking.** The always-on
  endpoints are just `/`, `/health`, `/protocol`, `/v1/models`, `/routes`,
  `/v1/messages`, `/v1/messages/count_tokens` (`src/server.rs:106-112`). An opt-in
  `[server.admin]` surface ([#77]) adds browser-based Anthropic-account provisioning
  and a **read-only account-pool dashboard** (`src/admin/mod.rs:87-103`), but there is
  still no request/token usage or cost accounting; observability beyond that is opt-in
  Sentry metrics only (`src/metrics.rs`). CLIProxyAPI ships a full management API +
  quota/usage manager and a third-party dashboard ecosystem; raine/claude-code-proxy
  ships a built-in **monitor TUI** (live sessions, active / recent requests, error
  events) — a live-traffic view shunt's account-oriented dashboard doesn't replace.
- **No own ChatGPT OAuth login.** shunt reuses the Codex CLI login
  (`~/.codex/auth.json`); a first-party PKCE flow is an open TODO
  (`src/auth/mod.rs:19-20`). raine/claude-code-proxy is prior art here — it ships its
  own `codex auth login` (PKCE) **and** `codex auth device` (device-code), so it works
  without the Codex CLI installed.
- **No plugin / interceptor system.** The adapter set is a fixed three-variant `match`
  (`src/proxy.rs:170-186`); CLIProxyAPI has a full plugin host (RPC ABI, auth
  providers, executor routing, request/response translators).
- **Plain HTTP only** (TLS out of scope, `docs/m4-inbound-auth.md:13`).

## 6. Improvement opportunities (from this comparison)

Ordered by fit with shunt's mission. **In-scope** items advance high-fidelity
translation / Claude-Code-native behavior; **scope-boundary** items would move shunt
toward being a fleet gateway and warrant a conscious decision first.

### In-scope

- **A. tool-search context savings (already tracked: [#43]).** shunt renders
  `tool_reference` as name-only `"Loaded tool: X"` text and forwards *all* deferred
  tool schemas upfront (`src/model/responses_request.rs:393-403,475-508`) — the loop
  works but reclaims zero context by default. Port the server-side emulation (filter
  deferred+unloaded tools, inject full schema on `tool_reference`) — reference implementation:
  CLIProxyAPI PR #1892 (`Adamcf123/CLIProxyAPI@main`). **Partially addressed by [#82]**:
  an opt-in `tool_search = true` per-provider flag now maps tool search onto the Responses
  API's native, client-executed `tool_search` protocol instead of the text shim, for a stock
  OpenAI or ChatGPT/Codex provider routing to a gpt-5.4+ model (see footnote 8 above). It's
  off by default pending a live probe of backend acceptance, so the shim (and the zero-savings
  gap for xAI/Grok and older models) remains the baseline until operators opt in.

- **B. Codex WS: live-probe the continuation normalization (already tracked: [#45]).** **Done (2026-07-13).**
  Reasoning/`function_call` normalization was schema-validated against 3 sources; a live probe over the
  WebSocket transport then captured real `message`/`reasoning`/`function_call` output items and diffed
  them against `normalize_item` — **no unaccounted field**. The probe corrected two assumptions: the
  backend omits reasoning `status` and returns an empty plaintext `content` array under `store:false`
  (both already stripped, so the match is unaffected). End-to-end, all three item kinds continued from
  `previous_response_id` on a warm pool (delta-only turns, zero `previous_response_not_found` rejects).
  A new `shunt.codex_continuation` counter (hit vs full-input fallback) makes future drift visible. The
  one residual is namespaced/MCP tool calls (need a live MCP server to trigger); their `namespace` strip
  stays schema-grounded until probed.

- **C. Codex WS: mid-stream failure fallback (resolved: [#46]).** Two fixes closed
  this. **[#93]** removed one *cause* at checkout: a half-open pooled socket now
  fails the reuse liveness probe (a timely `Pong` is required, not just a local
  write) and is replaced with a fresh handshake before the turn's frame is sent, so
  a stale connection can no longer break mid-stream. **[#46]** then closed the
  residual send→first-event window the checkout probe cannot cover (the socket dies
  *after* the frame is sent, before the first event): `open_ws_turn` peeks the first
  event and `commit_or_fallback` re-drives the turn over HTTP on a pre-first-event
  transport error (`src/adapters/responses/websocket.rs`), extending the pre-handshake
  safety net across that window. A failure *after* the first event has streamed is
  genuinely mid-stream — restarting would duplicate output — so it is surfaced as a
  clean Anthropic `error` SSE event rather than replayed; mid-turn resume via
  `previous_response_id` is a deliberate non-goal (partial output is already
  committed to the client). Covered by `tests/codex_websocket_fallback.rs`.

- **D. Codex WS: speculative prewarm (`generate:false`) (already tracked: [#47]).** Explicitly out of scope
  today (`docs/m7-codex-websocket.md:53-58`), but it is a real Codex latency
  optimization — prewarming the socket/context before the first token. Worth
  revisiting once continuation is live-probed.

- **E. Upstream retry/backoff (done: [#48]; safety-gated: [#126]).** The M4-planned bounded retry/backoff
  now ships as a reusable `src/retry.rs` layer shared by the Anthropic, Responses, and Cursor
  single-credential paths, strictly pre-stream, with exponential backoff + randomized jitter, honoring
  `Retry-After` in both its delta-seconds and HTTP-date forms (giving up cleanly when it exceeds
  budget). It always retries connection errors; a transient status `429`/`502`/`503`/`504`/`529`
  (Anthropic's "Overloaded") is retried only on the idempotent Cursor path, while the non-idempotent
  Anthropic Messages and single-credential Responses POSTs surface it immediately (#126: a response may
  mean the generation was already accepted upstream). It is configurable per provider under
  `[providers.<name>.retry]` (on by default, conservative), held off `count_tokens`, and left off the
  `claude_oauth`/`chatgpt_oauth` account pools, which drive their own account-rotation failover.

### Scope-boundary (decide before doing)

- **G. Minimal multi-account for ChatGPT/Codex — implemented, reactive-only.**
  M10 pools a handful of `~/.codex/auth.json`-style logins behind `chatgpt_oauth`
  with session-sticky selection and cooldown-based failover on 401/429/5xx/
  credential-resolution failure (`docs/m10-codex-multi-account.md`). What the
  original ask still lacks is the *fill-first* piece specifically: burning one
  account's rolling-window quota before moving to the next needs Codex to expose
  per-account rate-limit headers first (see H) — today the pool only reacts after a
  request has already failed on the current account, mirroring only the reactive
  half of the Anthropic (`claude_oauth`) pool's proactive-plus-reactive design.

- **H. Per-account quota/usage visibility.** Follows G. For Anthropic accounts the
  opt-in admin dashboard ([#77]) already surfaces each account's 5h/7d window and
  cooldown state (`src/accounts.rs:46-63`); extending the same per-account view to
  ChatGPT/Codex subscription accounts (as CLIProxyAPI's ecosystem does) is the part
  still missing. Ties to the observability gap.

- **I. Native Gemini (and other) backends.** Only relevant if shunt broadens past the
  Anthropic-Messages / OpenAI-Responses duality. Not currently in scope.

## 7. One-line takeaway

shunt is the **high-fidelity, Claude-Code-native** end of the spectrum. Its nearest
peer is **raine/claude-code-proxy** — same class (Rust,
subscription OAuth, per-`model` routing, Codex WS + `previous_response_id`
continuation) — against which shunt's edge is deeper continuation normalization,
Codex reasoning fidelity (raine drops it), an Anthropic-passthrough path (keep the
main session on Claude), and xAI OAuth; raine's edge is a built-in monitor TUI, a
first-party ChatGPT OAuth login, and Kimi breadth. Against **CLIProxyAPI**,
shunt wins on translation-path upload trimming (CLIProxyAPI's WS is a passthrough)
and trades away most fleet features (broad multi-account LB, management, plugins,
backend breadth) by design. It now provides a narrow Anthropic OAuth account pool
with model-aware proactive quota scheduling plus reactive failover, and a mirrored
ChatGPT/Codex pool that covers the reactive half only — quota-aware fill-first
rotation across Codex accounts remains a deliberate gap pending per-account
rate-limit visibility (§6, items G–H).
The highest-value in-scope work is finishing the tool-search
context savings ([#43]) — now partly addressed by an opt-in native `tool_search` path on
Codex/OpenAI ([#82]). The Codex WS transport's pre-first-event HTTP fallback gap
has since been closed ([#46]); its continuation-normalization live-probe ([#45])
remains open. The biggest deliberate gap left to weigh is quota-aware fill-first
rotation across the ChatGPT/Codex account pool — the pool itself now exists
(`docs/m10-codex-multi-account.md`, reactive-only), so only the proactive
fill-first half remains.

[#43]: https://github.com/pleaseai/shunt/issues/43
[#82]: https://github.com/pleaseai/shunt/issues/82
[#45]: https://github.com/pleaseai/shunt/issues/45
[#46]: https://github.com/pleaseai/shunt/issues/46
[#47]: https://github.com/pleaseai/shunt/issues/47
[#48]: https://github.com/pleaseai/shunt/issues/48
[#77]: https://github.com/pleaseai/shunt/issues/77
[#93]: https://github.com/pleaseai/shunt/issues/93
