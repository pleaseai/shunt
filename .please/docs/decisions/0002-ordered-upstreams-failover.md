# ADR-0002: Ordered upstreams with cross-provider failover

## Status

Accepted

## Date

2026-07-20

## Context

ADR-0001 added a per-provider `upstream_model` map to `[[models]]` entries and
restricted it to exactly one provider, reserving the map shape as the extension
point for ordered cross-provider failover (issue #218). Any multi-provider map
needs a defined attempt order, and ADR-0001 requires that order to be declared
explicitly rather than inferred from the unordered `ProvidersConfig` table.

The reference Claude apps gateway resolves this with a single mechanism: a
top-level ordered `upstreams:` list. Each list entry is one named, individually
credentialed route to a backend (`name`, `provider`, per-entry `auth`), the
list's declaration order is the global failover precedence, and a model's
`upstream_model` map — keyed by upstream *name*, defaulting to the provider
string when `name:` is omitted — only determines which upstreams participate.
There is no per-model ordering. Operators express account-level failover by
declaring several upstreams for the same provider (for example provisioned
throughput first, on-demand second, a second account third) and interleaving
them freely with other providers.

shunt's existing shape conflicts with that model in two ways:

1. `[providers.<name>]` is an unordered map, so it cannot carry a failover
   order.
2. Credentials are provider-scoped: `auth` is a string mode with sibling
   `api_key_env` / `api_key_header` fields and an `accounts` pool inside the
   provider. A failover chain layered on top of provider-internal account
   pooling produces two overlapping rotation mechanisms and cannot express the
   reference's interleaved, account-granular ordering (account 1 → other
   provider → account 2).

shunt is pre-1.0 and prioritized for internal use: breaking configuration
changes are acceptable provided the migration path is exact.

## Decision

Introduce a single ordered top-level `[[upstreams]]` array. One entry is one
failover unit: a named route with its own credential scope. Declaration order
is the global failover precedence.

```toml
[[upstreams]]
name = "anthropic-1"
provider = "anthropic"                                # preset
auth = { mode = "claude_oauth", account = "acct-1" }

[[upstreams]]
name = "kimi"
provider = "kimi"
auth = { mode = "api_key", env = "MOONSHOT_API_KEY" }

[[upstreams]]
name = "anthropic-2"
provider = "anthropic"
auth = { mode = "claude_oauth", account = "acct-2" }

[[upstreams]]
name = "codex"
provider = "codex"                                    # auth defaults to chatgpt_oauth

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
anthropic-1 = "claude-opus-4-8"
kimi = "kimi-k2"
anthropic-2 = "claude-opus-4-8"
codex = "gpt-5.2"
# chain: anthropic-1 → kimi → anthropic-2 → codex ([[upstreams]] order; map order is irrelevant)
```

The decision has four parts:

1. **Ordered `[[upstreams]]` list.** `name` is required, non-empty, and unique.
   A model's failover chain is the `[[upstreams]]` declaration order filtered
   to the names appearing in its `upstream_model` map; an upstream not named
   in the map does not participate. ADR-0001's single-provider restriction is
   lifted for configurations declared through `[[upstreams]]`.

2. **`auth` becomes string-or-map.** The map form absorbs the previous
   provider-level `api_key_env`, `api_key_header`, and `accounts` siblings and
   adds credential *scoping* for OAuth modes: `account = "x"` (single),
   `accounts = [...]` (subset, full `AccountConfig` tables allowed), or no
   narrowing (the whole account store; for `chatgpt_oauth` an empty store falls
   back to the single-account `~/.codex/auth.json` path — today's pool
   behavior). The string form `auth = "claude_oauth"` remains valid shorthand
   for `{ mode = "claude_oauth" }`. The account pool is therefore preserved as the
   *intra-upstream* selection mechanism (quota-aware rotation, burn-rate
   avoidance, storm control), while the chain governs *inter-upstream*
   transitions.

3. **Provider presets.** The optional `provider` field names a built-in preset
   — a static data table supplying `kind`, `base_url`, and the default auth
   mode (and default API-key env var). No preset overrides `count_tokens`; it
   keeps its normal per-upstream default. Initial set:
   `anthropic`, `codex`, `openai`, `xai` (developer API), `grok`
   (subscription CLI proxy), `kimi` (Anthropic-compatible endpoint), `cursor`.
   Explicitly set entry fields always override the preset; omitting `provider`
   and setting `kind` + `base_url` manually remains fully supported for
   arbitrary compatible gateways. An unknown preset name is a configuration
   error listing the available presets.

4. **Physical-account state sharing.** Per-account runtime state (quota
   windows, health, cooldowns, in-flight admission) is keyed by store family
   plus stable account identity, not by upstream name. Identity follows the
   existing `account_identity()` rule — account UUID when present, name as
   fallback — with the UUID read from the account's resolved credentials (the
   store entry or inline credential material), not from the static TOML view
   alone, so an explicit `account = "x"` reference and a store scan resolving
   to the same entry coalesce. Cross-upstream sharing requires this verified
   identity: state is shared only between accounts carrying the same verified
   UUID/account id or resolving to the same store entry. UUID-less inline
   accounts are upstream-scoped (name
   fallback namespaced by upstream name), so same-named inline accounts with
   different credentials never merge. Selection scope is per upstream;
   verified-account truth is global, so several upstreams referencing the same
   account observe one coherent quota/health state and storm-control admission
   counts. The `[server.pool]` persistence file's key schema changes
   accordingly; as a warm-start cache it is version-bumped and incompatible
   files are ignored (one cold start).

Failover runtime semantics mirror the reference gateway: statuses 429, 401,
403, 404, and 5xx, plus pre-response-header network failures, advance to the
next upstream while remembering the best failure (preference 429 → 401/403 →
404 → other 5xx); any other status returns immediately; when nothing was
remembered a `502 api_error` "all upstreams failed (N attempted)" is
synthesized; there is no failover once a 2xx response's headers have been
returned. Responses carry `x-gateway-upstream`, `x-gateway-model`, and
`x-gateway-upstream-model` headers. Inbound auth remains optional exactly as
today. When inbound auth is configured, its gating check considers the whole
chain: the request is gated if any chain member injects credentials
(`auth != passthrough`), not just the first; client-credential stripping is
deferred to per-upstream dispatch so a `passthrough` chain member still
receives the client's original headers.

**Backward compatibility.** The legacy `[providers.<name>]` table keeps
parsing unchanged. Each legacy provider becomes one implicit upstream named
after its table key, loaded in name-sorted order (identical to today's
`BTreeMap` behavior), so existing configurations — including their
single-provider `upstream_model` maps, whose keys are provider names — retain
their routing and selection semantics. Two deliberate exceptions apply under
both config forms. Legacy providers that reference the same physical account
now share per-account runtime state, which was previously provider-scoped;
this is a deliberate behavior change shipped with this feature and called out
in migration. In addition, every proxied response now carries the additive
`x-gateway-upstream`, `x-gateway-model`, and `x-gateway-upstream-model`
metadata headers. The legacy form has no defined order, so a multi-provider
`upstream_model` map combined with it is a configuration error whose message
shows the exact rewrite. Declaring both `[[upstreams]]` and any
`[providers.*]` table is an error ("use exactly one"). Migration to ordered
failover is therefore opt-in and mechanical.

## Consequences

### Positive

- One structure expresses provider order, account-granular interleaving, and
  credential scope; the reference gateway's operational patterns (provisioned
  first, overflow second, separate account third, different vendor last) map
  directly.
- The pool's proactive quota-aware rotation is preserved inside each upstream;
  pool exhaustion (relayed `401`/`429`/`5xx` or translated `502`) naturally
  triggers chain advancement without a special case.
- Presets remove `base_url`/auth boilerplate for the supported backends while
  the manual `kind` + `base_url` escape hatch keeps arbitrary compatible
  gateways first-class.
- Existing configurations keep working without edits; migration is a
  mechanical, documented rewrite.
- Ordering is structural (an array is ordered) yet explicit — adopting
  `[[upstreams]]` is a deliberate step, satisfying ADR-0001's constraint.

### Negative

- Two provider-declaration syntaxes coexist until the legacy table is retired;
  documentation must maintain both.
- Same-kind upstreams duplicate connection-level settings (`base_url`,
  `retry`, `websocket`, …) per entry; presets and defaults mitigate but do not
  eliminate this.
- The account-state re-keying changes the pool persistence schema and the
  admin snapshot shape, and requires a one-time cold start.
- Internal identifiers (`ProviderConfig`, `Route.provider`, pool keying by
  `route.provider`) diverge from the new user-facing "upstream" vocabulary
  until a follow-up rename.

### Neutral

- `[[routes]]`, `[[route_prefixes]]`, and `server.default_provider` keep their
  semantics and reference the same name namespace under either declaration
  form.
- The inbound Codex endpoint (`[server.codex_endpoint]`) stays pinned to a
  single named upstream and does not participate in failover.
- Discovery (`GET /v1/models`) is unaffected; participation in a chain is a
  routing concern.

## Alternatives Considered

- **Per-model `provider_order` list on `[[models]]`:** Rejected. The reference
  has no per-model ordering; a per-model list adds a second source of truth
  and cannot express account-granular interleaving.
- **Top-level `provider_order = [...]` referencing `[providers.<name>]`:**
  Rejected. Keeps ordering apart from the declarations it orders, still
  provider-granular, and drifts from the reference shape.
- **Ordered `[[providers]]` array-of-tables (same key, dual shape):** Rejected.
  Overloading one key with map-or-array parsing complicates deserialization
  and error messages; a distinct `upstreams` key gives clean legacy
  coexistence and matches the reference's `upstreams:`/`upstream_model` naming
  pair exactly.
- **Two layers — `[providers.*]` as connection definitions plus `[[upstreams]]`
  entries referencing them (`provider = "anthropic"`, `account = "acct-1"`):**
  Rejected. Preserves connection-setting sharing but keeps two coupled
  config surfaces and an indirection the reference proves unnecessary;
  presets recover the boilerplate savings without the extra layer.
- **Inferring order from `[providers.<name>]` declaration order (IndexMap):**
  Rejected, reaffirming ADR-0001 — invisible semantics, and a
  table-sorting formatter would silently change failover order.
- **Full flattening without pools (upstream = exactly one credential, as in
  the reference):** Rejected. Discards shunt's quota-aware multi-account
  rotation (#135, #195) in favor of purely reactive status-class failover; the
  auth-map scoping keeps both mechanisms with a crisp boundary.
