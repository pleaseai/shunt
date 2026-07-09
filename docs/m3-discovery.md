# M3 — Model discovery & selection UX (spec)

> Companion to [`implementation-plan.md`](implementation-plan.md) §2 and §7 of
> [`m1-responses-translation.md`](m1-responses-translation.md). Covers `discovery.rs`,
> `codex/models.rs`, and the optional `count_tokens` endpoint. M3 is UX polish — the gateway
> already works via `ANTHROPIC_CUSTOM_MODEL_OPTION` without it. Source of truth for the wire
> contract: [LLM Gateway Protocol § Model discovery](https://code.claude.com/docs/en/llm-gateway-protocol#model-discovery).

## 1. Scope

1. Serve `GET /v1/models` so Claude Code can populate the `/model` picker from shunt.
2. Provide the model map + reasoning-effort table (`codex/models.rs`) referenced by M1 §5/§7.
3. Optionally serve `POST /v1/messages/count_tokens` for exact context accounting (else Claude
   Code estimates locally — M0 already passes this through to Anthropic).

## 2. Two ways to select a mapped model (document both)

| Path | Who uses it | Constraint |
| :-- | :-- | :-- |
| **`ANTHROPIC_CUSTOM_MODEL_OPTION="<id>"`** (primary) | any non-Claude id, e.g. `gpt-5.2-codex` | none — id validation is skipped, any string the gateway routes works |
| **`GET /v1/models` discovery** (`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`) | Claude-named aliases | Claude Code **ignores ids not beginning with `claude`/`anthropic`** |

→ For OpenAI/Codex ids (`gpt-*`), discovery will not surface them; `ANTHROPIC_CUSTOM_MODEL_OPTION`
is the documented default. Discovery is only useful if shunt exposes a **Claude-named alias**
(e.g. `claude-opus-via-codex`) that routes to a `gpt-*` upstream.

## 3. `GET /v1/models` — wire contract (must obey)

- Request: `GET /v1/models?limit=1000`, **3-second timeout**, **any redirect is treated as
  failure** (even `http`→`https`). Serve it **directly** at the configured base URL — no proxy
  hop, no redirect, fast.
- Auth: exactly **one** credential header — `ANTHROPIC_AUTH_TOKEN` as bearer if set, else the
  resolved API key in `x-api-key`. (Differs from inference, which sends both.) Accept both;
  M-scope: do not require auth to succeed for a local gateway, but read it if present.
- Response body:
  ```json
  { "data": [ { "id": "claude-opus-via-codex", "display_name": "Opus (via Codex)" } ] }
  ```
  Claude Code reads `id` + optional `display_name`; ignores non-`claude`/`anthropic` ids.
- Claude Code caches results to `~/.claude/cache/gateway-models.json` and refreshes each
  startup; on failure it falls back to the cached/built-in list. So a slow or redirecting
  `/v1/models` degrades silently — keep it instant.

## 4. shunt implementation (`discovery.rs`)

- Serve the entries from a config `[[models]]` list:
  ```toml
  [[models]]
  id = "claude-opus-via-codex"      # must start with claude/anthropic to be honored
  display_name = "Opus (via Codex)"
  ```
- Return `{ "data": [...] }` verbatim from config; no upstream call.
- Never redirect; respond well under 3 s.
- If `[[models]]` is empty, return `{ "data": [] }` (discovery simply adds nothing; the custom
  model option still works).
- A discovered id should also have a matching `[[routes]]` entry (id → provider + `upstream_model`)
  so selecting it actually routes; validate this linkage at config load (warn if a `[[models]]`
  id has no route).

## 5. Model map + effort (`codex/models.rs`)

> **Stale-slug warning (verified 2026-07-09):** the `gpt-*-codex` names below (from
> `insightflo`) are **rejected** by the live ChatGPT Codex backend. Real usable slugs are
> account/plan-entitled (e.g. a free account → `gpt-5.5`) and fetched live by the codex CLI.
> Treat this table as reference only; prefer passing the model through / `upstream_model`.
> See [`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md) §0.


Referenced by M1 §5/§7. Provide:
- `map_model(id, route) -> upstream_model`: prefer the route's `upstream_model`; else a table
  fallback (`gpt-5.2 → gpt-5.2-codex`, etc.); else the id unchanged.
- `effort_for(model, route, thinking) -> "low|medium|high|xhigh"` per M1 §5 order:
  config override → model→effort table → model-name suffix (`-xhigh|-high|-medium|-spark|-low`)
  → `thinking.enabled ? high : medium`.
- Keep the reference tables (from `insightflo/chatgpt-codex-proxy/src/codex/models.ts`) in this
  one module so a Codex-CLI model list bump touches a single file.

## 6. `count_tokens` (implemented)

`POST /v1/messages/count_tokens` passes through to the upstream for Anthropic-routed models.
For a `responses`-routed model there is no exact Responses token-count endpoint, so
`proxy::forward` short-circuits before the adapter based on the provider's `count_tokens` setting:
`estimate` (default) returns **404** and Claude Code estimates locally (the protocol allows this
for an absent endpoint), while `tiktoken` returns an approximate local o200k_base count as
`{"input_tokens": N}` (see `src/count_tokens.rs`). Either way a count request is never turned into
(and billed as) a full inference call. Covered by `count_tokens_returns_404_for_responses_model`
and `count_tokens_uses_tiktoken_when_enabled` in `tests/passthrough.rs`.

## 7. Interactions to document

- `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` is required for discovery; off by default.
- The `availableModels` managed setting bounds what discovery can add (delivered via MDM /
  managed settings, not server-managed on gateway configs).
- A discovered explicit id that resolves to the same model as a built-in alias folds into the
  built-in row (Claude Code ≥ v2.1.197) — expected, not a bug.

## 8. Tests

- `/v1/models` returns configured entries; empty list when unconfigured; never emits a redirect;
  responds without an upstream call.
- Config validation: a `[[models]]` id lacking a `[[routes]]` entry warns.
- `map_model` / `effort_for` table + suffix + override precedence (pure unit tests).
