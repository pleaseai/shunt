# Ordered upstreams and cross-provider failover

Engineering spec for issue #218, implementing [ADR-0002](../.please/docs/decisions/0002-ordered-upstreams-failover.md).
Status: **design accepted, implementation pending.** This document is the
implementation contract; deviations discovered during implementation must be
recorded here in the same PR.

The reference Claude apps gateway's behavior cited throughout was extracted
from the shipped `claude` binary (2.1.215): the config schema (zod), the
per-upstream model mapping, and the failover loop. Where this spec says
"reference contract", shunt must match it observably.

## 1. Configuration schema

### 1.1 `[[upstreams]]` — ordered failover units

A new top-level array. Declaration order is the global failover precedence.
One entry is one failover unit: a named route with its own credential scope.

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Unique, non-empty, non-whitespace. The identity used by `upstream_model` maps, `[[routes]] provider`, `server.default_provider`, metrics, and the admin surface. |
| `provider` | no | Built-in preset id (§1.2). Supplies defaults for `kind`, `base_url`, and `auth`; no preset overrides `count_tokens` (§1.2). |
| `kind` | if no preset | Adapter protocol, unchanged enum (`anthropic`, `responses`, `cursor`). |
| `base_url` | if no preset | As today. |
| `auth` | no | String or map (§1.3). Default: preset's default auth, else `passthrough`. |
| other provider fields | no | `effort`, `count_tokens`, `websocket`, `tool_search`, `retry` — unchanged semantics, now per upstream. |

Explicit fields always override preset-supplied values.

### 1.2 Provider presets

A static data table in code (table-driven per AGENTS.md; no per-provider
branching). Unknown preset name → config error listing available presets.

| preset | kind | base_url | default auth |
|---|---|---|---|
| `anthropic` | anthropic | `https://api.anthropic.com` | `passthrough` |
| `codex` | responses | ChatGPT/Codex backend (current documented URL) | `chatgpt_oauth` |
| `openai` | responses | `https://api.openai.com/v1` | `api_key`, env `OPENAI_API_KEY` |
| `xai` | responses | `https://api.x.ai/v1` | `api_key`, env `XAI_API_KEY` |
| `grok` | responses | `https://cli-chat-proxy.grok.com/v1` | `xai_oauth` |
| `kimi` | anthropic | `https://api.moonshot.ai/anthropic` | `api_key`, env `MOONSHOT_API_KEY` |
| `cursor` | cursor | Cursor backend (current documented URL) | `cursor_oauth` |

Exact URLs must be taken from the existing adapters/docs at implementation
time, not from this table if they drift.

No preset overrides `count_tokens`: every upstream keeps the field's normal
serde default (`tiktoken`), which is only meaningful for `responses` and
`cursor` kinds — `anthropic`-kind upstreams (including `kimi`) always forward
`count_tokens` requests upstream regardless of the setting. Operators override
per upstream with the explicit `count_tokens` field as today.

### 1.3 `auth` — string or map

Serde untagged: a bare string `"claude_oauth"` is shorthand for
`{ mode = "claude_oauth" }`. The map is internally tagged by `mode` (the
existing `AuthMode` strings) and absorbs the legacy sibling fields:

| mode | map fields |
|---|---|
| `passthrough` | — |
| `api_key` | `env` (required unless preset supplies it), `header` (default as today) |
| `claude_oauth` / `chatgpt_oauth` | optional scope: `account = "name"` (single) **or** `accounts = [...]` (subset; entries are full `AccountConfig` tables or bare name strings referencing the store). No scope → whole store scan; for `chatgpt_oauth` an empty store additionally falls through to the single-account `~/.codex/auth.json` path (both preserved today's behavior). Setting both `account` and `accounts` is an error. |
| `xai_oauth` / `cursor_oauth` | as today (no scoping fields yet) |

Unknown map keys for a mode are errors (strict), mirroring the reference's
strict per-provider auth objects.

### 1.4 Legacy `[providers.<name>]` compatibility

- Parses exactly as today (string `auth` + `api_key_env`/`api_key_header`/
  `accounts` siblings). Sibling fields remain valid **only** in the legacy
  table; inside `[[upstreams]]` the auth map is the only spelling.
- After load, each legacy provider becomes one implicit upstream named after
  its table key, in name-sorted order (today's `BTreeMap` iteration), flagged
  *unordered*.
- Declaring both `[[upstreams]]` and any `[providers.*]` → hard error: use
  exactly one.
- A multi-entry `upstream_model` map with legacy (unordered) providers → hard
  error whose message shows the exact rewrite (`[providers.codex]` →
  `[[upstreams]]` + `name = "codex"`). No silent alphabetical failover.
- `SHUNT_` env-var overrides keep addressing entries by name under both forms.

### 1.5 Validation summary (new rules)

1. `upstreams[].name`: required, non-empty, non-whitespace, unique.
2. Unknown `provider` preset → error listing presets.
3. Without a preset, `kind` and `base_url` are required (as today).
4. `auth` map: mode-specific strict fields; `account` xor `accounts`.
5. Both `[[upstreams]]` and `[providers.*]` present → error.
6. `upstream_model` maps: existing per-entry rules (non-empty key/value, known
   upstream name, no `[1m]` suffix, no `[[routes]]` conflict) unchanged;
   multi-entry maps additionally require ordered `[[upstreams]]`.
7. Same-kind upstreams under distinct names are legal; nothing may assume at
   most one upstream per kind or per auth mode.

## 2. Chain resolution (routing)

- For a model whose `upstream_model` map has N entries: the chain is the
  `[[upstreams]]` declaration order filtered to names that are keys of the
  map. Map order is irrelevant. An upstream not named in the map does not
  participate (no builtin-catalog fallback — stricter than the reference,
  deliberate).
- Single-entry maps and map-less models resolve exactly as today (one-element
  chain; `[[routes]]`, prefixes, default provider unchanged).
- `Route` gains nothing; `resolve_request` returns an ordered non-empty list
  of `Route`s (`resolve_request_chain`); existing single-route callers use the
  first element.
- The `[1m]` context-window-hint stripping and effort defaulting apply per
  chain element as today.

## 3. Failover loop (proxy) — reference contract

Wraps adapter dispatch in `forward()`. Per attempt, in chain order:

1. Dispatch to the element's adapter. The response status is known before the
   body streams (lazy body), so inspecting it buffers nothing.
2. **Advance to the next element** when: the adapter returns a relayed
   response with status `429`, `401`, `403`, `404`, or any `5xx`; or the
   adapter fails before returning response headers (connect/TLS/header-phase
   network failure, translated 502). This is the failover boundary: streaming
   adapters return at header time with a lazy body, so a transport failure
   after 2xx headers but before or during the body surfaces to the client as
   today and does not re-enter the chain. Adapters must preserve the raw
   upstream status or failure class through `AdapterError`, so this
   classification happens before client-facing status mapping. Today's
   Responses and Cursor error mappings both flatten `404` to `502` via
   `client_facing_status`; both must be extended to carry the raw class. Remember the best failure so far with preference
   `429 → 401/403 → 404 → other 5xx`; drop superseded remembered responses
   unread.
3. **Return immediately** on any other status (2xx, 400, …).
4. Once a 2xx response's headers have been returned to the client there is no
   failover; any subsequent body/transport failure surfaces to the client as
   today.
5. Chain exhausted: return the remembered best failure; if none was
   remembered, synthesize `502 api_error` with message
   `all upstreams failed (N attempted)` in the Anthropic error shape.

Response headers on every proxied response (success or final failure):
`x-gateway-upstream` (upstream name), `x-gateway-model` (client-requested id),
`x-gateway-upstream-model` (mapped upstream id).

Cross-cutting:

- **Inbound auth gating**: inbound auth remains optional exactly as today. When
  inbound auth is configured, the gating check considers the whole chain: the
  request is gated if *any* chain element injects credentials
  (`auth != passthrough`), not just the first. Credential stripping moves with
  it: today `check_inbound_auth` removes the client's `authorization` /
  `x-api-key` headers up front, which in a mixed chain would destroy the
  credentials a `passthrough` element must forward. In a chain the gate only
  authenticates; stripping (and the `x-shunt-inbound-client` stamp) is
  deferred to per-upstream dispatch and applied only when dispatching to an
  element that injects credentials, so a `passthrough` element still receives
  the client's original headers. (The anti-spoof removal of inbound-only
  headers from unauthenticated requests stays at the gate.)
- **count_tokens**: answered from the first chain element, as a chain has one
  advertised id; no failover for count_tokens.
- **Metrics**: per-attempt `record_proxied_request` labeled by upstream name,
  plus a failover counter (attempted/advanced/exhausted) so dashboards can see
  chain pressure. Exact metric name settled at implementation.
- **`[server.codex_endpoint]`**: out of scope; stays pinned to its configured
  upstream.

## 4. Pool and account state

The pool survives as the **intra-upstream** mechanism; the chain is the
**inter-upstream** mechanism.

- An upstream's auth scope defines its pool membership: whole store, subset,
  or single account. Selection (`select_order`, quota-aware rotation,
  burn-rate avoidance, storm control) runs unchanged within that membership.
- Pool exhaustion already surfaces as a relayed advance-class status
  (`401`/`429`/`5xx`) or a translated `502` — all are advance classes in §3,
  so exhaustion flows into the chain without a special case.
- **Physical-account state sharing**: per-account state (quota windows,
  health, cooldowns, in-flight admission counts, refresh locks) is re-keyed
  from provider name to *(store family, stable account identity)*. Identity
  follows the existing `account_identity()` rule — account UUID when present,
  name as fallback — with the UUID read from the account's *resolved*
  credentials (the store entry or inline credential material, e.g. the Claude
  account UUID or `chatgpt_account_id`), not from the static TOML view alone,
  so an explicit `account = "acct-1"` reference and a whole-store scan that
  resolve to the same store entry produce the same key and coalesce.
  Cross-upstream sharing requires this verified identity: two upstreams share
  an account's state only when their accounts carry the same verified
  UUID/account id or resolve to the same store entry (bare-name store
  references). A UUID-less inline `AccountConfig` table is upstream-scoped: its
  fallback key is namespaced by the upstream name, so same-named inline
  accounts with different credentials under different upstreams never merge.
  (Today's name fallback is safe only because the key includes the provider
  name; an un-namespaced global name key would merge distinct accounts.)
  Selection scope remains per upstream.
- `[server.pool]` stays global. The `state_path` persistence file's key schema
  changes; bump its schema/version marker and ignore incompatible files (one
  cold start; it is a warm-start cache). Same for the gateway-sessions file if
  its keys embed provider names (verify at implementation).
- `usage_refresh_seconds` polling iterates physical accounts deduplicated
  across upstreams.
- Admin dashboard: snapshot per upstream (its selection scope); an account
  referenced by several upstreams appears under each, backed by the same
  shared state.

## 5. Migration

Documented user-facing in the site guide; summarized here.

- **No action** for existing configs: the legacy table keeps parsing unchanged,
  implicit upstreams inherit provider names, and existing `upstream_model` maps
  (keyed by those names) retain their routing and selection semantics. Two
  deliberate exceptions apply under both config forms. Legacy providers that
  reference the same physical account now share per-account runtime state,
  which was previously provider-scoped; this is a deliberate behavior change
  shipped with this feature and called out in migration. In addition, every
  proxied response now carries the additive `x-gateway-upstream`,
  `x-gateway-model`, and `x-gateway-upstream-model` metadata headers.
- **To adopt failover**: rewrite each `[providers.<name>]` table as an
  `[[upstreams]]` entry (`name = "<name>"`, same fields; fold
  `api_key_env`/`api_key_header`/`accounts` into the `auth` map), order
  entries by preference, then add entries to the model's `upstream_model`
  map. Presets typically shrink entries to `name` + `provider` (+ auth scope).
  Caution: preset auth defaults apply only when the entry omits `auth` — e.g.
  the `kimi` preset reads `MOONSHOT_API_KEY` (Moonshot's official convention),
  while older shunt examples used `KIMI_API_KEY`; either export the new name
  or keep the old one explicitly via
  `auth = { mode = "api_key", env = "KIMI_API_KEY" }`.
- Pool persistence file: discarded once on upgrade (cold start), by version
  bump.

## 6. Implementation surface

| Area | Files |
|---|---|
| Schema, presets, auth map, validation, implicit upstreams | `src/config.rs` (split new modules if it would exceed the 500-line file guidance: e.g. `src/config/upstreams.rs`, `src/config/presets.rs`) |
| Chain resolution | `src/routing.rs` |
| Failover loop, headers, gating, metrics | `src/proxy.rs`, `src/metrics.rs`, `src/adapters/mod.rs`, `src/adapters/responses/error.rs`, `src/adapters/cursor/mod.rs`, `src/model/responses.rs` |
| Account-state re-keying, auth-map credential resolution, persistence version bump | `src/accounts.rs`, `src/state_persist.rs`, `src/usage_poll.rs`, `src/adapters/anthropic/mod.rs`, `src/adapters/responses/pool.rs`, `src/auth/mod.rs`, `src/admin/` |
| Tests | config/routing unit tests; `tests/passthrough.rs`-style wiremock integration tests for chain order, advance classes, best-failure preference, header parity, gating, raw status/failure-class propagation through every adapter path |

Internal identifiers (`ProviderConfig`, `Route.provider`, `route.provider`
pool keys) keep their names in this change; renaming to `Upstream*` is a
follow-up.

## 7. Out of scope / follow-ups

- Builtin model-catalog fallback for upstreams missing from a model's map
  (reference behavior; shunt stays strict).
- User-defined preset tables.
- Auth-scope fields for `xai_oauth` / `cursor_oauth`.
- Internal `Provider*` → `Upstream*` rename.
- Legacy `[providers.*]` deprecation timeline (none planned pre-1.0).
- First-body-frame peek before committing a 2xx to the client, so
  header-successful-but-immediately-dead upstreams could still fail over
  (precedent: the Codex WS first-event peek fallback).
