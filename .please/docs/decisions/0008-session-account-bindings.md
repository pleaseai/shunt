# ADR-0008: Assigned session-to-account bindings replace hash-derived stickiness

## Status

Accepted

## Date

2026-10-31

## Context

A pooled session's account is a function of its session id. `select_order_inner`
hashes `x-claude-code-session-id` (or Codex `session-id`) with SHA-256 into a slot
over the distinct account identities (`src/accounts.rs`, `stable_session_index`).
It uses that slot's account while the account is healthy and under its soft
threshold. Otherwise it falls back along an order every displaced session shares:
priority, then headroom, or the soonest reset under `sort_by_reset`.
[`docs/m8-anthropic-multi-account.md`](../../../docs/m8-anthropic-multi-account.md)
promises the mapping is deterministic across restarts while the account list is
unchanged. #857, merged since, replaces the slot with a rendezvous ranking of the
distinct identities (`accounts::session_score`) and orders a displaced session's
fallbacks by that ranking instead of headroom. The account is still a function of
the session id.

The hash has three consequences, measured on a six-account Claude Max pool over
11 hours ([#873](https://github.com/pleaseai/shunt/issues/873), comment by @shaharsha):

1. **Displaced sessions move as a herd.** The fallback order is shared, so every
   displaced session sits on the same account and moves together whenever the head
   changes: 29 switches in 12 sessions, 18 of them with a warm cache, and only 3
   of those 18 after a 429.
2. **Sessions return home on recovery.** The hash brings a session back to its
   home account as soon as that account is healthy again, which is another move.
3. **Policies that choose an account cannot be expressed.** Draining the weekly
   window that resets first (#843), fill-first and least-loaded all want to choose
   the account. The hash already has, so #843 had to drop per-session accounts
   entirely.

A move loses account-bound state: Sonnet 5.5 and Haiku 5.5 thinking blocks are
dropped on another account, the prompt cache starts cold, and Codex WebSocket and
`previous_response_id` continuation are per account.

## Decision

### 1. A session is bound, not hashed

One binding table maps `(configured provider, SHA-256(canonical session identity))`
to a physical account identity (aliases coalesce as they do today) and a last-used
time. The hashed identity is the selection key each path passes today: the effective
session id on the Anthropic and outbound Responses paths, and `{client}:{session}`
on the inbound Codex endpoint. A raw session id is
never stored, logged or exported. Sessionless requests keep round-robin and create
no binding.

### 2. A binding moves only when its account cannot serve

A binding is created when an account first serves the session successfully (a 2xx),
not on the first request. It moves only when the bound account is ineligible for the
current request **and** another account serves that request successfully, or when the
bound account was attempted on this request and failed over to another account that
served it; the binding then records the account that served. Ineligible
means: cooling down for this request's scope (global, Fable or model), hard-over,
quota-rejected, capped, paused for the provider, disabled, or no longer configured.

- An Anthropic 429 without a `rejected` quota status keeps classifying as
  `PauseSame`. It waits on the same account and does not move the binding.
- If no other account can serve, the binding stays where it is.
- A recovered account, or a change in priority, leaves an eligible binding in
  place; a priority change affects only later placements and moves.
  Nothing moves back.
- A request-scoped limit (Fable-only `7d_oi`, a per-model cooldown) moves the
  binding on the request it applies to, not on requests for other models.

**Soft-near hysteresis.** An account that is merely past its soft threshold must
stay soft-near continuously for 60 seconds before a bound session leaves it, and
only when a healthy alternative exists. Any non-near observation resets the count.
Every other ineligibility moves the binding immediately.

### 3. Selection policies become assignment strategies

`[server.pool] assignment` picks the strategy run when a binding is created or
moved:

| Value | Placement | On a move |
| :-- | :-- | :-- |
| `spread` (default) | Top of #857's rendezvous (HRW) ranking (`session_score`) over the eligible distinct identities, priority ignored | Highest-ranked eligible identity within the best available priority tier |
| `soonest_weekly_reset` | Priority, then governing weekly reset ascending (unknown last), then configuration order | Same ordering among eligible identities |

Rendezvous hashing keeps `spread` restart-stable without persistence. Adding an
account moves about 1/(n+1) of new placements instead of most. Each displaced
session also gets its own fallback order, which breaks up the herd. Bindings
layer on #857's ranking rather than replace it: the ranking places new and
displaced sessions, and behind a session's head it orders the in-request
fallbacks in place of headroom.

`[server.pool] sort_by_reset` and its admin override stay. Their effect narrows to
the in-request failover order and sessionless traffic, and they never place or
move a binding. Making it an alias of `soonest_weekly_reset` was rejected: it
sorts by the minimum of the 5-hour and weekly resets, not the governing weekly
reset, so an alias would silently change its meaning.

### 4. Bounds are constants

Bindings expire after 24 hours idle. The table holds at most 10,000 bindings across
all providers, evicting the least recently used, which matches the existing
`MAX_POOL_ENTRIES` / `MAX_WINDOW_ENTRIES` caps in `codex_ws.rs`. Over about
three days of #873's data, a 24-hour TTL would have moved 2 of 6,391 resumptions.
Config keys are deferred until the eviction counter shows they are needed.

### 5. One table for every pooled path

The Anthropic pool, the outbound Responses/Codex pool and the inbound Codex
endpoint share the table. The Codex WebSocket pool keeps keying sockets per
conversation and account, but takes the account from the binding instead of
placing the conversation itself. Providers with different configured names stay
separate even when they share physical accounts.

### 6. Persistence follows the pool's quota state

- **State file:** the v3 envelope gains an optional `bindings` array, and the
  version stays 3. The envelope does not reject unknown fields, so the previous
  release reads a new file, ignores the bindings, and keeps its quota on a
  downgrade. A version bump would cold-start it instead. v2 files and v3 files
  without the field load with no bindings.
- **Store:** a new migration in both engines adds `pool_session_binding(provider,
  session_hash, account_key, last_used)`, with a composite primary key and an
  index on `last_used`. It is written as deltas, not as a whole-table replace, and
  the one-time import carries bindings.
- **Write cadence:** the existing 15-second write-behind, and the store's existing
  flush at shutdown; file mode has no shutdown flush today and gains none here. A
  binding's persisted `last_used` advances at most once an hour, so a busy session
  does not rewrite its row on every flush. That is ample precision for a 24-hour
  TTL.
- **Neither configured:** memory only. Rendezvous placement stays stable across
  restarts, but sessions that had been moved are placed again.
- **Restore:** expired bindings, and bindings whose account is no longer
  configured, are dropped.

### 7. Observability

Each binding move increments `shunt.pool.session_account_switches{reason}`
(`proactive`, `cooldown`, `failover`; #872), and evictions are counted. `GET
/admin/api/pool` reports bound sessions per physical identity, so concentration is
visible.

### 8. Concurrency

No lock is held across an await point. Commits carry a generation: a slower
request that finishes later cannot overwrite a newer binding. The binding commits
on a successful serve, so admission bypass or an opportunistic re-probe alone does
not move it.

## Consequences

### Positive

- Draining strategies become expressible without moving in-flight sessions.
- Sessions on a degraded account pay one move, not one per change at the head,
  and they do not move back.
- Displaced sessions spread over their own rendezvous orders instead of piling
  onto one head account.

### Negative

- **Breaking, documented:** M8's "deterministic across restarts while the account
  list is unchanged" becomes "stable across restarts by persistence, or by
  rendezvous placement when persistence is off". Upgrading reshuffles placement
  once, because HRW differs from `hash mod n`.
- New state to persist, migrate and bound. A crash loses at most 15 seconds of
  moves. Once a store runs migration `0002`, the previous release refuses it
  (`SchemaTooNew`). Rolling back with `[store]` therefore needs the store
  restored, while a state file stays readable by the previous release.
- A session that alternates between models with incompatible limits can still move
  more than once. One binding per session, independent of model, cannot rule
  that out.

### Neutral

- An account's load now reflects history. A newly recovered account fills only
  as new sessions arrive, and `GET /admin/api/pool` shows the skew.

## Alternatives Considered

- **No per-session account** (#843 as written): drains correctly, but every
  change at the head moves every session.
- **Keep the hash; add policies to the fallback order only** (as `sort_by_reset`
  does): sessions stay spread, so nothing drains.
- **Rendezvous hashing plus a per-session fallback order without bindings**
  (#857): merged, and kept as the ranking `spread` places by, but rejected as a
  replacement for bindings. It cuts switches 29 → 16 in the #873 replay, but
  sessions still return home on recovery and draining policies stay
  inexpressible.
- **In-memory bindings only:** a restart moves every displaced session. That is
  a regression from today's restart-stable hash for anyone who persists quota.
- **Configurable TTL and cap from day one:** more public surface before any data
  justifies it.

## Related

- Issue [#873](https://github.com/pleaseai/shunt/issues/873), #843, #856, #857, #872
- [ADR-0006](0006-optional-pluggable-store.md) — the store bindings persist in
- Track `session-account-binding-20261031`
