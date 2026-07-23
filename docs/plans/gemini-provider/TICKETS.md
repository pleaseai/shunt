# TICKETS — Native Gemini provider (Path B, Google Code Assist backend)

Status keys: `TODO` · `WIP` · `DONE` · `BLOCKED`. `depends-on` gates dispatch.
Parallelizable = no shared files / no dep. Each ticket = one implementer agent,
then one reviewer pass before commit.

**Goal:** make `gemini-3.1-pro-preview` and `gemini-3-flash-preview` routable model
ids through the shunt gateway so they can be spawned as first-class agents in
multi-vendor fan-outs/panels, alongside the existing `gpt-*`/`grok`/`kimi`
shunt-gateway agents.

**Backend (live-verified 2026-07-21):** `https://cloudcode-pa.googleapis.com/v1internal:{loadCodeAssist,generateContent,streamGenerateContent?alt=sse}`,
`Authorization: Bearer <google ya29 oauth token, cloud-platform scope>`. Same
backend Antigravity/`agy` uses (via gRPC) — not tied to the standalone gemini CLI.
Request = Code Assist wrapper `{"model","project":<cloudaicompanionProject>,"request":{<vanilla Gemini GenerateContentRequest>}}`.
Response wrapped `{"response":{<GenerateContentResponse>},"traceId","metadata"}`.
SSE = `data: {wrapped}\n\n` per chunk, final chunk carries `finishReason`+`usageMetadata`,
**no `[DONE]` sentinel**; SSE-endpoint errors return a plain JSON body (not an SSE frame).

**Key design decision (v1):** auth reuses the existing on-disk Google token
(`~/.gemini/oauth_creds.json`, refreshed via the public gemini-cli/Code-Assist
`client_id`) — the fast, Codex-style path (`chatgpt_oauth` reads `~/.codex/auth.json`).
Own-PKCE `shunt login google` (A2) was **dropped** — Google refuses new logins on
the public gemini-cli client, so shunt stays coupled to the CLI-refreshed token file
for v1; the documented fallback if that refresh dies is an AI Studio API key (adds
cost, unbuilt). **S1 passed** (see `logs/2026-07-21_S1-auth-spike.md`): the public
client_id refreshes a self-held token end-to-end today.

**Constraints:** own `feat/` branch + tracking issue (do NOT build on
`feat/214-admin-live-activity`). Preserve streaming semantics; files <500 lines;
table-driven not hardcoded provider logic; gateway-owned errors in the Anthropic
error shape; full protocol/translation test coverage; `cargo fmt` + `clippy -D warnings` + tests green.

---

## Stream S — De-risk

### TICKET-S1 — Auth feasibility spike · TODO · depends-on: none · wave 1 · agent: claude
**Problem:** the whole plan assumes shunt can obtain/refresh a Google OAuth token for the Code Assist backend without the gemini/agy binaries. Prove it before building auth.
**Do:** using the on-disk `refresh_token` and the public gemini-cli/Code-Assist `client_id` (+ installed-app secret, both public), refresh via `oauth2.googleapis.com/token`, then call `loadCodeAssist` and one `generateContent`. Record the working client_id/secret/scope, token-file locations, and refresh cadence. Decide go/no-go; if refresh is rejected, document the fallback (promote A2 own-PKCE login, or read-only reuse of a CLI-refreshed file).
**Files:** throwaway (scratch only — no repo changes).
**Done when:** a shunt-held refresh yields a `ya29` token that returns HTTP 200 from `loadCodeAssist`; findings + go/no-go written to `logs/`.

## Stream C — Config foundation

### TICKET-C1 — Provider/auth/adapter enums + validation + seeding · TODO · depends-on: none · wave 1
**Problem:** shunt has no Gemini provider kind, Google auth mode, or Gemini adapter kind.
**Do:** add `ProviderKind::Gemini` (`src/config.rs`), `AuthMode::GoogleOauth`, `AdapterKind::Gemini` + `From<ProviderKind>` mapping (`src/routing.rs`); `host_is_google_codeassist()` origin leak-guard mirroring `host_is_chatgpt`/`host_is_cursor`; validation errors `GoogleOauthWrongKind`/`GoogleOauthNonGoogleHost`/`GoogleOauthNotHttps` mirroring the `XaiOauth*`/`ChatgptOauth*` variants (google_oauth must be kind=gemini, https, Google host); default provider seeding for `gemini-3.1-pro-preview` + `gemini-3-flash-preview` pointing at the Code Assist host.
**Files:** `src/config.rs`, `src/routing.rs`.
**Done when:** a `[providers.gemini]` config parses+validates; misconfig (wrong kind / non-Google host) is rejected; `cargo test` config+routing unit tests green.

## Stream A — Auth

### TICKET-A1 — Google auth module (token resolve/refresh + project discovery) · TODO · depends-on: S1, C1 · wave 2
**Problem:** the adapter needs a valid bearer + Code Assist project id per request.
**Do:** `src/auth/google/{mod,auth}.rs` — resolve a Google OAuth token from `~/.gemini/oauth_creds.json` (handling atomic write ENOENT races gracefully), single-flight refresh on `expiry_date` via the public client_id, and discover+cache `cloudaicompanionProject` via `loadCodeAssist`. Add `Credential::GoogleOauth(String)` to `src/auth/mod.rs`. On refresh 401, return a clear error guiding the user to re-run `gemini login`. Template: `src/auth/cursor/auth.rs`.
**Files:** `src/auth/google/mod.rs`, `src/auth/google/auth.rs`, `src/auth/mod.rs`.
**Done when:** `src/auth/mod.rs` exposes `Credential::GoogleOauth`, returns a valid bearer+project, refreshes on expiry under concurrency, and handles missing/stale token files cleanly; unit tests with mock token endpoint + mock loadCodeAssist pass.

### TICKET-A2 — `shunt login google` own-PKCE flow (v2 hardening) · DROPPED
**Reason:** Google has sunsetted new interactive logins on the public gemini-cli client ID ("client no longer supported for Gemini Code Assist for individuals"). Self-refresh of existing `~/.gemini/oauth_creds.json` tokens works and is handled by A1. Documented fallback if tokens are revoked is AI Studio API key.

## Stream B — Translation

### TICKET-B1 — Request translation (Anthropic Messages → Gemini generateContent) · TODO · depends-on: C1 · wave 2
**Problem:** Anthropic request body (`MessagesRequest`) must be translated into Gemini's `GenerateContentRequest` shape.
**Do:** `src/model/gemini_request.rs` — convert Anthropic system prompt, messages (user/assistant text, images, tool_use, tool_result), generationConfig (temperature, max_tokens, stop_sequences, and nested thinkingConfig when explicitly requested), and tools (FunctionDeclarations) into Gemini JSON format. Keep `GenerateContentRequest` decoupled so it can be unit-tested without needing the Code Assist outer envelope (`{model, project, request}` wrapped by D1).
**Files:** `src/model/gemini_request.rs`, `src/model/mod.rs`.
**Done when:** unit tests cover message role mapping, tool definitions, tool results, thinking budget, and edge cases (empty messages/system prompts).

### TICKET-B2 — Response + SSE translation (Gemini → Anthropic events) · TODO · depends-on: B1 · wave 3
**Problem:** the wrapped Gemini response/stream must become Anthropic Messages events.
**Do:** `src/model/gemini.rs` — non-stream response → Anthropic message; SSE `data:` chunks → `content_block_delta`(text_delta), `functionCall`→`tool_use` blocks, `finishReason`→`message_delta.stop_reason`, `usageMetadata`→usage; thinking parts → thinking blocks; handle no-`[DONE]` stream end. Precedent: `src/model/responses.rs`.
**Files:** `src/model/gemini.rs`.
**Done when:** golden tests turn captured SSE frames (see `logs/`) into the correct Anthropic event sequence for text, tool-call, and finish; non-stream path covered.

## Stream D — Adapter

### TICKET-D1 — Gemini adapter + proxy dispatch · TODO · depends-on: A1, B2, C1 · wave 4
**Problem:** nothing wires auth + translation into a live forwarding path.
**Do:** `src/adapters/gemini/{mod,client}.rs` implementing `Adapter` — inject bearer+project (A1), build request (B1), POST to `:streamGenerateContent?alt=sse` (or `:generateContent` for non-stream), re-frame via B2, map errors to the Anthropic error shape **including the SSE-error-as-plain-JSON case**, reuse `src/retry.rs` for 429/5xx backoff (standard-tier throttles hard). Register the arm in `src/proxy.rs` `match route.adapter` and the `count_tokens` guard; declare in `src/adapters/mod.rs`.
**Files:** `src/adapters/gemini/mod.rs`, `src/adapters/gemini/client.rs`, `src/adapters/mod.rs`, `src/proxy.rs`.
**Done when:** an end-to-end mock test streams a Gemini turn out as Anthropic SSE; a 429 backs off then surfaces cleanly.

## Stream T — Tests

### TICKET-T1 — Integration test suite · TODO · depends-on: D1 · wave 5
**Problem:** protocol changes need focused coverage (AGENTS.md boundary).
**Do:** `tests/gemini_translation.rs` + `tests/gemini_protocol.rs` — request shaping, SSE re-framing, error mapping (JSON-body error), tool round-trip, non-stream.
**Files:** `tests/gemini_translation.rs`, `tests/gemini_protocol.rs`.
**Done when:** `cargo test --all-features --workspace` green including the new tests.

## Stream X — Docs

### TICKET-X1 — Docs across all surfaces · DONE · depends-on: D1 · wave 5

## Stream G — Agent layer (the actual user goal)

### TICKET-G1 — Gateway routing + Gemini agent definitions · DONE · depends-on: D1 · wave 5
**Problem:** the point is Gemini as a spawnable agent in the multi-vendor teams.
**Do:** configure the running shunt gateway to route `gemini-3.1-pro-preview` + `gemini-3-flash-preview` to the gemini provider and authenticate it (reuse the CLI token via A1 — A2 own-login was dropped); create `~/.claude/agents/gemini-3-1-pro.md` and `~/.claude/agents/gemini-3-flash.md` mirroring `~/.claude/agents/grok.md` (frontmatter `model:` = the slug, spawn WITHOUT a model override).
**Files:** `~/.claude/agents/gemini-3-1-pro.md`, `~/.claude/agents/gemini-3-flash.md`, running gateway config.
**Done when:** spawning the `gemini-3-1-pro` agent in a session routes through shunt and returns a real Gemini completion; verified live.

## Stream E — Concurrency hardening (v2)

### TICKET-E1 — Multi-Google account pool + proactive 429 rotation · TODO · depends-on: D1 · wave 5
**Problem:** standard-tier rate limits (few req/min per model) cap concurrent Gemini seats to ~1–2 per account; fan-outs need more.
**Do:** wire the Gemini provider into the account-pool machinery (`src/accounts.rs`) for multi-account selection + cooldown/backoff on 429, mirroring the `chatgpt_oauth` pool; split load across the two model slugs (separate quota buckets).
**Files:** `src/accounts.rs`, `src/adapters/gemini/*`.
**Done when:** the pool rotates across N Google accounts on 429; a parallel-agent fan-out no longer serially 429s with ≥2 accounts configured.
