# shunt — Implementation Plan (Rust)

> Status: **draft plan for implementation**. Planned with Opus; implementation to be
> handed to the `codex-rescue` skill. "Frozen decisions" and "Module layout" are a
> near-spec; "Open decisions" still need sign-off before the affected milestone starts.

## 1. What we are building

`shunt` is a **spec-compliant Claude Code LLM Gateway** (Anthropic Messages format). Claude
Code is pointed at it via `ANTHROPIC_BASE_URL`. For each inference request shunt looks at the
**`model` field** and routes to the provider configured for that model — passing through to
Anthropic unchanged for every model it isn't told to divert.

**Phase 1 target providers: OpenAI / Codex / ChatGPT.** That is, drive OpenAI-family models
from inside Claude Code by translating **Anthropic Messages ⇄ OpenAI Responses API**, over
two credential paths:

- **`openai`** — OpenAI Platform API (`api.openai.com`), authenticated with an **API key**.
- **`codex` / `chatgpt`** — the ChatGPT-subscription-backed Codex endpoint
  (`chatgpt.com/backend-api/codex/responses`), authenticated with a **ChatGPT OAuth token**.

Both speak the **Responses API** shape, so there is a single translation core with two thin
transport/auth backends on top.

### No per-agent classification

An earlier design diverted traffic **per agent** by fingerprinting each subagent's system
prompt (the fragile heuristic `seifghazi/claude-code-proxy` uses). **Dropped.** Claude Code
already assigns models per agent client-side — subagent `model:` frontmatter, the `/model`
picker, `CLAUDE_CODE_SUBAGENT_MODEL`, `ANTHROPIC_CUSTOM_MODEL_OPTION`. By the time a request
reaches shunt it already carries the chosen `model` ID, so shunt only maps **`model` →
provider**. Simpler, robust (no prompt-shape coupling), and still selective per-model rather
than a global swap.

### User-facing flow

1. Operator configures shunt with providers and a model→provider map.
2. Developer makes the target model selectable in Claude Code:
   - `ANTHROPIC_CUSTOM_MODEL_OPTION="gpt-5.2-codex"` (skips ID validation — any string the
     gateway accepts works; **primary path for non-Claude IDs**), or
   - `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` to populate `/model` from shunt's
     `GET /v1/models` (only surfaces ids beginning with `claude`/`anthropic`).
3. Developer selects that model (main session, or per-agent via frontmatter /
   `CLAUDE_CODE_SUBAGENT_MODEL`).
4. Requests for that model divert to OpenAI/Codex/ChatGPT; everything else passes through.

## 2. The gateway contract we must satisfy

Source of truth: **[LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol)**.
Non-negotiable requirements for an `ANTHROPIC_BASE_URL` gateway:

| Requirement | Detail |
| :-- | :-- |
| **Endpoints** | `POST /v1/messages` (required), `POST /v1/messages/count_tokens` (optional — else Claude Code estimates locally), `GET /v1/models` (discovery), `HEAD /` (probe → 200). Match on **path**; inference posts to `/v1/messages?beta=true`. |
| **Streaming** | Responses **must** stream SSE incrementally; buffering the whole response stalls the client. |
| **Forward unchanged** | `anthropic-version` and `anthropic-beta` request headers **byte-for-byte** — never allowlist beta values (the set changes every release). Relevant only for the Anthropic pass-through path; the OpenAI path consumes them. |
| **Credential** | Inbound `Authorization`/`x-api-key` carry the developer's *gateway* credential; shunt **consumes** them and substitutes the *target provider's* credential outbound. |
| **Don't modify bodies (Anthropic path)** | Rewriting/redacting the body breaks the beta-header ↔ body-field pairing → hard `400`. Changing `model` is the one legitimate mutation. (The OpenAI path fully rebuilds the body, so this applies to pass-through only.) |
| **Error forwarding** | Forward upstream error **status + body unmodified**; Claude Code's retry/degradation matches on upstream error wording. shunt's *own* errors are returned in Anthropic error shape. |
| **Attribution block** | Claude Code prepends a client-version + conversation fingerprint to the system prompt; shunt must **not strip it** (developer's call via `CLAUDE_CODE_ATTRIBUTION_HEADER=0`). Stable per-conversation since Claude Code v2.1.181. |
| **Consume for observability** | `x-claude-code-session-id`, `x-claude-code-agent-id`, `x-claude-code-parent-agent-id` — logging/attribution only; not used for routing. |

### Model discovery (`GET /v1/models`)

- `GET /v1/models?limit=1000`, **3 s timeout**, **redirects = failure** — serve directly at the
  base URL.
- Auth on this endpoint: one credential header — `ANTHROPIC_AUTH_TOKEN` bearer if set, else the
  resolved API key in `x-api-key`.
- Response `{ "data": [ { "id", "display_name"? } ] }`; Claude Code **ignores ids not beginning
  with `claude`/`anthropic`.** → Non-Claude ids (e.g. `gpt-5.2-codex`) reach the picker via
  `ANTHROPIC_CUSTOM_MODEL_OPTION`, not discovery. Discovery is a convenience for Claude-named
  aliases only.

## 3. Architecture

```
                    ANTHROPIC_BASE_URL
Claude Code  ─────────────────────────────▶  shunt
  (unchanged tool loop, skills, headers)        │  route by model id
                                                │
                                                ├─▶ anthropic  (pass-through, default)
                                                │
                    ┌───────────────────────────┤  Anthropic Messages ⇄ OpenAI Responses
                    │  shared translation core   │
                    ▼                            ▼
        openai backend                     codex / chatgpt backend
        api.openai.com/v1/responses        chatgpt.com/backend-api/codex/responses
        auth: API key                      auth: ChatGPT OAuth bearer + chatgpt-account-id
```

The two OpenAI-family backends differ only in **endpoint + auth + model map**; the
Anthropic⇄Responses translation is shared. Model this as an `Adapter` trait with a
`ResponsesAdapter` used by both, parameterized by a `Credential` and `Endpoint`.

> A future, optional path (**not phase 1**): an `anthropic` pass-through adapter to a
> downstream Anthropic-compatible gateway (LiteLLM/Portkey/bifrost) that does provider
> translation for us. Kept in the design because the default route already needs
> pass-through, but no non-Anthropic work depends on it.

## 4. Authentication (phase-1 detail)

Reuse the credentials `codex login` already writes to **`~/.codex/auth.json`** rather than
implementing a fresh OAuth flow. Confirmed shape: `{ auth_mode, OPENAI_API_KEY, tokens{...},
last_refresh }`.

| Backend | Credential source | Outbound headers |
| :-- | :-- | :-- |
| `openai` | `OPENAI_API_KEY` (env, or from `~/.codex/auth.json`, or config) | `Authorization: Bearer <key>` |
| `codex` / `chatgpt` | ChatGPT OAuth `tokens` from `~/.codex/auth.json` (access + refresh + account id) | `Authorization: Bearer <access>` + `chatgpt-account-id: <id>` |

ChatGPT OAuth specifics (from `insightflo/chatgpt-codex-proxy`):

- Endpoints: `https://auth.openai.com/oauth/authorize`, `https://auth.openai.com/oauth/token`;
  loopback redirect `http://localhost:1455/auth/callback`; PKCE; fixed Codex `client_id`.
- **Auto-refresh** with `grant_type=refresh_token` when `expires_in` is near/past; persist the
  new tokens back.
- `chatgpt-account-id` comes from the JWT claim `https://api.openai.com/auth →
  chatgpt_account_id` (base64-decode the JWT payload; no signature verification needed to read
  it).

Design:

- **Primary:** a `TokenStore` that reads `~/.codex/auth.json`, auto-refreshes, and (optionally)
  writes back — so `codex login` is the setup step, no separate `shunt login`.
- **Fallback:** a `shunt login` implementing the same PKCE loopback flow, writing shunt's own
  token file, for environments without the codex CLI.
- Crates: `oauth2` (PKCE + refresh), a loopback `axum` callback server, `base64` + `serde_json`
  for the JWT payload, file lock for concurrent refresh.

## 5. Technology stack (Frozen decisions)

| Concern | Choice | Rationale |
| :-- | :-- | :-- |
| Async runtime | **tokio** | standard; required by axum/reqwest |
| HTTP server | **axum** | ergonomic; first-class streaming (`Body::from_stream`, `Sse`) + serves the OAuth loopback |
| HTTP client | **reqwest** (rustls, `stream`) | `bytes_stream()` for incremental SSE; no OpenSSL |
| Serialization | **serde** + **serde_json** | `#[serde(flatten)]` round-trips unknown fields on the pass-through path |
| SSE | **eventsource-stream** (parse) | required for the Responses adapter; pass-through relays raw bytes |
| OAuth | **oauth2** + **base64** | PKCE refresh; decode JWT account-id |
| Config | **serde + figment** (TOML + env) | layered, typed |
| CLI | **clap** (derive) | `shunt --config`, `shunt --check`, `shunt login` |
| Logging | **tracing** + **tracing-subscriber** | per-request spans keyed on session id |
| Errors | **thiserror** (lib) + **anyhow** (boundaries) | |
| Tests | **wiremock** + **insta** + `tokio::test` | mock upstreams; snapshot converters |
| Toolchain | Rust stable, edition 2021, `cargo` | + `orca.yaml` / `.worktreeinclude` per repo convention |

## 6. Module layout (handoff target for codex)

```
shunt/
  Cargo.toml
  shunt.toml.example
  orca.yaml                # cargo/mise-based; + .worktreeinclude (.env*, .claude/settings.local.json)
  src/
    main.rs                # clap: run | check | login ; init tracing, load+validate config
    config.rs              # Config structs, figment load, validation, model→provider index
    error.rs               # ShuntError -> Anthropic-shaped JSON error response
    server.rs              # axum Router + AppState (config, http client, model index, token store)
    proxy.rs               # buffer request, parse routing view, pick provider+adapter, stream back
    routing.rs             # model_id -> Provider (explicit map → prefix rules → default)
    discovery.rs           # GET /v1/models
    headers.rs             # forward-unchanged vs consume vs strip; credential injection
    auth/
      mod.rs               # Credential enum { ApiKey, ChatGptOAuth }; TokenStore trait
      codex_auth.rs        # read/refresh ~/.codex/auth.json; account-id from JWT
      login.rs             # `shunt login`: PKCE loopback flow (fallback)
    adapters/
      mod.rs               # trait Adapter { prepare_request; adapt_response_stream }
      anthropic.rs         # pass-through (no translation) — default route
      responses.rs         # Anthropic Messages ⇄ OpenAI Responses API (shared by openai+codex)
    codex/
      models.rs            # model map (claude/gpt id -> codex model) + reasoning effort
    model/
      anthropic.rs         # partial request view { model, system?, #[serde(flatten)] rest } + response/SSE types
      responses.rs         # OpenAI Responses request / output items / SSE event types
  tests/
    passthrough.rs         # header forwarding, model rewrite, SSE relay, error passthrough (wiremock)
    responses_translate.rs # insta snapshots: request + streaming response conversion
    discovery.rs           # /v1/models shape + id filtering
    auth.rs                # token load/refresh; account-id extraction
```

### Notable implementation points

- **Buffer request, stream response.** Bodies are safe to buffer to parse `model`; responses
  must relay as an async byte stream so SSE reaches Claude Code incrementally.
- **Translation core (`responses.rs`)** — the phase-1 bulk (reference sizing:
  `fuergaosi233` ~264+385 LOC for Chat Completions; `insightflo` request converter ~474 LOC
  for Responses). Must cover: system + messages (roles, multimodal/text blocks), tool
  definitions + `tool_choice`, tool-call ↔ `function_call` items, `stop_reason`/finish
  mapping, `max_tokens`, and `thinking` → `reasoning.effort`. Streaming: consume Responses SSE
  events and **re-emit Anthropic SSE** (`message_start`, `content_block_*`, `message_delta`,
  `message_stop`, `ping`) — a stateful converter, snapshot-tested.
- **Pass-through path (`anthropic.rs`)** — minimal mutation: change only `model` + credential
  header; preserve every other field via `#[serde(flatten)] rest`.
- **Header handling** — forward `anthropic-version`/`anthropic-beta` verbatim on the Anthropic
  path; inject provider credential; strip hop-by-hop; recompute `content-length`.
- **Error shape** — shunt's own failures → Anthropic error JSON; upstream errors relayed
  unmodified (translate OpenAI error → Anthropic error shape on the Responses path so Claude
  Code recovers gracefully).

## 7. Configuration schema (draft — `shunt.toml`)

```toml
[server]
bind = "127.0.0.1:3001"
default_provider = "anthropic"          # main session / unmapped models pass through

[providers.anthropic]
adapter  = "anthropic"                  # pass-through
base_url = "https://api.anthropic.com"

[providers.openai]
adapter     = "responses"               # Anthropic ⇄ OpenAI Responses API
base_url    = "https://api.openai.com/v1"
auth        = "api_key"                 # OPENAI_API_KEY (env / ~/.codex/auth.json / config)

[providers.codex]                        # ChatGPT-subscription backend
adapter     = "responses"
base_url    = "https://chatgpt.com/backend-api"   # + /codex/responses
auth        = "chatgpt_oauth"           # reuse ~/.codex/auth.json tokens (auto-refresh)

# ---- Model routing (explicit first, then prefixes) ----
[[routes]]
model = "gpt-5.2-codex"; provider = "codex"      # selected via ANTHROPIC_CUSTOM_MODEL_OPTION
[[routes]]
model = "gpt-5.1"; provider = "openai"

[[route_prefixes]]
prefix = "gpt-"; provider = "codex"

# ---- Discovery (only claude/anthropic-prefixed ids surface in the picker) ----
# [[models]] id = "claude-opus-via-codex"  display_name = "Opus (via Codex)"
```

Env overrides through figment (e.g. `SHUNT_SERVER__BIND`); credentials resolved by the auth
layer, not stored in the file.

## 8. Milestones

| # | Deliverable | Exit criteria |
| :-- | :-- | :-- |
| **M0** | **Transparent pass-through proxy** | `HEAD /`→200; `POST /v1/messages` buffers, forwards to Anthropic with headers+credential correct, relays SSE incrementally; upstream errors byte-for-byte. Conformant do-nothing gateway — validates plumbing. |
| **M1** | **Anthropic ⇄ OpenAI Responses translation + `openai` (API key)** | Config + `routing.rs`; the `responses` adapter (request + **streaming** response converter, tool calls, reasoning effort); drive an OpenAI model end-to-end from Claude Code with `OPENAI_API_KEY`. **Core translation lands here.** |
| **M2** | **`codex` / `chatgpt` backend (ChatGPT OAuth)** | Reuse M1 translation; add `TokenStore` over `~/.codex/auth.json` (auto-refresh) + `chatgpt-account-id` header + `chatgpt.com/backend-api/codex/responses`; `shunt login` fallback. Delivers OpenAI-family via ChatGPT subscription. |
| **M3** | **Model map + discovery UX** | `codex/models.rs` (id→codex model + effort); `GET /v1/models`; document `ANTHROPIC_CUSTOM_MODEL_OPTION` path; optional `count_tokens`. |
| **M4** | **Hardening + observability** | tracing spans on session id; timeouts, upstream retry/backoff, graceful shutdown; optional request capture behind a flag; optional `GET /protocol`. |

MVP for the stated goal = **M0 + M1 + M2** (drive OpenAI **and** Codex/ChatGPT). M3 is UX
polish; M4 is production-readiness.

## 9. Testing strategy

- **Unit:** config parse/validate; `routing.rs` order; header rules; auth token load/refresh +
  account-id extraction; converters via `insta` snapshots on captured real payloads.
- **Integration (`wiremock`):** fake Anthropic upstream — assert `anthropic-beta`/`-version`
  verbatim, credential swap, `model` rewrite with nothing else changed, **incremental** SSE
  relay, unmodified error body. Fake Responses upstream — assert request translation and that
  streamed Responses events become well-formed Anthropic SSE.
- **Golden replay:** capture real Claude Code request bodies (streaming + tool-use + thinking),
  replay through shunt against mocks.
- **Live smoke (manual/opt-in):** point Claude Code at a running shunt with a real
  `OPENAI_API_KEY` (M1) and a real `codex login` session (M2); run a tool-using task.
- **Conformance:** if feasible, boot a real Claude apps gateway, fetch `GET /protocol`, diff
  behavior.

## 10. Open decisions (need sign-off)

- **D1 — RESOLVED.** Phase 1 = native Anthropic⇄OpenAI **Responses API** translation; providers
  `openai` (API key) + `codex`/`chatgpt` (ChatGPT OAuth). Thin-router/downstream-gateway path
  deferred/optional.
- **D2 — RESOLVED.** Phase 1 targets the **Responses API** only (what Codex/ChatGPT requires and
  OpenAI's current surface). No Chat Completions adapter in phase 1.
- **D3 — RESOLVED.** Reuse `~/.codex/auth.json` as the primary credential store, with
  **read + auto-refresh + write-back** allowed (refresh persists new tokens like the codex CLI
  does); `shunt login` as fallback for environments without it. Use a file lock to avoid
  clobbering concurrent refreshes by the codex CLI.
- **D4 — RESOLVED.** Config format is **TOML**.
- **D5 — RESOLVED.** **Stateless by default**; request/response capture only behind an opt-in
  flag (M4).

## 11. References (read during planning)

- **LLM Gateway Protocol** — the contract: <https://code.claude.com/docs/en/llm-gateway-protocol>
- **Add a custom model option** — `ANTHROPIC_CUSTOM_MODEL_OPTION`:
  <https://code.claude.com/docs/en/model-config#add-a-custom-model-option>
- `insightflo/chatgpt-codex-proxy` — **direct phase-1 reference**: Anthropic `/v1/messages` →
  ChatGPT Codex backend; ChatGPT OAuth (auth.openai.com, loopback :1455, refresh, account-id
  from JWT), endpoint `chatgpt.com/backend-api/codex/responses`, Responses-API request/response
  transformers, model map + reasoning effort; TypeScript.
- `fuergaosi233/claude-code-proxy` — Anthropic⇄OpenAI translation sizing (request ~264 /
  response+SSE ~385 LOC); Python.
- `seifghazi/claude-code-proxy` — per-agent prompt-hash routing (the heuristic we are *not*
  adopting) + SQLite request monitor; Go.
- `~/.codex/auth.json` — reusable credential store written by `codex login`
  (`auth_mode`, `OPENAI_API_KEY`, `tokens`, `last_refresh`).
```
