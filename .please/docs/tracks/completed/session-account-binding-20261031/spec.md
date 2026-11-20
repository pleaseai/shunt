# Session-to-Account Bindings for Pooled Providers

> Track: session-account-binding-20261031
> Issue: #873 · Decision record: [ADR-0008](../../../decisions/0008-session-account-bindings.md)

## Overview

A pooled session's account is a function of its session id today: the id is hashed
into a slot over the pool's distinct account identities. When that account cannot
serve, every displaced session falls back along one shared order, so they all land
on the same account and move together whenever the account at the head of that
order changes. The measurements on #873 show this: 29 account switches across
12 sessions in 11 hours, 18 of them while the prompt cache was still warm, and only
3 of those 18 followed a 429 within 60 s.

A move costs more than a cache miss. Thinking blocks are tied to the account that
produced them, the prompt cache starts cold on the new account, and Codex
WebSocket and `previous_response_id` continuation are per account.

This track replaces the hash-derived account with an **assigned binding**. A
session is bound to one account the first time it needs one, and it stays bound
until that account can no longer serve it. Selection policies become **assignment
strategies**, which run only when a session needs an account. Today's even spread
is kept as the default strategy, `spread`. The second strategy,
`soonest_weekly_reset`, lets the operator drain the weekly window that resets
first (#843) without moving every session each time the head changes.

Operators of multi-account Claude subscription pools and Codex pools benefit: sessions keep their
cache, thinking and continuation state; quota-draining policies become possible;
and how concentrated the pool is becomes visible.

## Requirements

### Functional Requirements

- [ ] FR-1: bind each pooled session to one account identity on its first request, keyed by the provider and a one-way hash of the session's canonical identity, never the raw session id
- [ ] FR-2: serve a bound session's requests from its bound account while that account can serve the request
- [ ] FR-3: move a binding only when the bound account cannot serve the current request and another account serves it successfully. The bound account cannot serve when it is ineligible for the request — cooling down for the request's scope (global, Fable or model), past its hard threshold, quota-rejected, capped, paused, disabled, no longer configured, or soft-near past the FR-5 debounce — or when the attempt on it fails over to another account
- [ ] FR-4: keep a moved session on its new account when the previous account recovers, and apply priority or recovery changes to new assignments only
- [ ] FR-5: debounce soft-near: a session leaves an account that is merely past its soft threshold only after that state has held continuously for 60 seconds, and only when a healthy alternative exists
- [ ] FR-6: provide an assignment strategy setting under `[server.pool]` named `assignment` with the values `spread` (default) and `soonest_weekly_reset`
- [ ] FR-7: under `spread`, place a new session by rendezvous hashing over the eligible distinct identities, ignoring priority on first placement as today does; on a move, pick the highest-weight eligible identity within the best available priority tier
- [ ] FR-8: under `soonest_weekly_reset`, place and move by priority, then the governing weekly reset ascending with unknown last, then configuration order
- [ ] FR-9: expire a binding after 24 hours without use, and cap the table at 10,000 bindings across all providers by evicting the least recently used
- [ ] FR-10: share one binding table across the Anthropic pool, the outbound Responses/Codex pool and the inbound Codex endpoint; the Codex WebSocket pool reuses the binding rather than placing sessions itself
- [ ] FR-11: persist bindings with the pool's quota state, in the `[server.pool] state_path` file or in `[store]` when that is configured; with neither, keep them in memory only
- [ ] FR-12: on restart or reload, discard bindings that have expired or whose account is no longer in the pool, and assign those sessions again when they next arrive
- [ ] FR-13: count each binding move in `shunt.pool.session_account_switches` with `provider`, `reason` and `thinking_bound` labels (`thinking_bound`: whether the request's model produces account-bound thinking blocks), and count evictions. The `selection` reason #872 reserves for a policy-only move does not occur, because a binding never moves for policy alone (FR-3). The reasons follow #872: `proactive` (the bound account was near its threshold past the FR-5 debounce, hard-over, capped, paused or disabled), `cooldown` (it was cooling down for the request's scope), `failover` (an in-request retry after an upstream failure served the request, or the bound account left the pool — AC-018)
- [ ] FR-14: report the number of bound sessions per physical account identity on `GET /admin/api/pool`
- [ ] FR-15: keep round-robin for sessionless requests (no session id, or an empty one), which create no binding
- [ ] FR-16: keep `[server.pool] sort_by_reset` and its admin override, with its effect narrowed to ordering in-request failover and sessionless traffic; it never places or moves a binding

### Non-functional Requirements

- [ ] NFR-1: no lock is held across an await point. A slower request that finished later never overwrites a newer binding
- [ ] NFR-2: the `account_pool_*` and stage-router benchmarks show no regression beyond CodSpeed's threshold. Bench coverage is added for binding hit, miss, eviction and concurrent move
- [ ] NFR-3: a state file written before this change (v2, or v3 without bindings) still loads with no bindings; a file written after it stays readable by the previous release, which ignores the bindings and keeps its quota; an existing store migrates forward without manual steps
- [ ] NFR-4: README, `docs/m8-anthropic-multi-account.md`, the M10 Codex docs, `docs/m11-inbound-codex-endpoint.md`, `docs/store.md`, `docs/m9-admin-surface.md`, the metric tables in `docs/running.md` and `docs/dashboard-metrics.md`, and the `site/` configuration reference, endpoints reference, pool guide and OpenTelemetry guide (English plus `ko`/`ja`/`zh-cn`) describe bindings and the switch metric in the same PR

## Acceptance Criteria

- [ ] AC-001 (FR-1): The system shall not write a raw session id into a binding, whether the binding is held in memory, in the state file or in the store, nor into a log line or metric emitted for a binding.
- [ ] AC-002 (FR-2, FR-4): When a session is bound to account A, A then cools down, and later recovers, the system shall keep serving that session from its new account B after A's recovery.
- [ ] AC-003 (FR-3): When the Anthropic upstream returns a 429 that does not carry a `rejected` quota status, the system shall retry on the same account and keep the binding unchanged.
- [ ] AC-004 (FR-3): If the bound account is ineligible for the request and no other account can serve it, then the system shall keep the binding unchanged and record no switch.
- [ ] AC-005 (FR-3): When a Fable request finds its bound account limited only by the Fable-scoped weekly window, the system shall serve that request from another account and bind the session to it.
- [ ] AC-006 (FR-5): While the bound account has been soft-near for less than 60 seconds, the system shall keep serving the session from it.
- [ ] AC-007 (FR-5): When a bound account reports a non-near assessment before 60 seconds of soft-near have elapsed, the system shall restart the 60-second count from zero.
- [ ] AC-008 (FR-5): When the bound account goes past its hard threshold, is paused, disabled, capped or cooling down, the system shall move the session on that request without waiting.
- [ ] AC-009 (FR-7): When an account is added to a pool of n accounts under `spread`, the system shall place new sessions so that about 1/(n+1) of them land on the added account.
- [ ] AC-010 (FR-7): When an account is added to a pool under `spread`, the system shall leave every existing binding where it is.
- [ ] AC-011 (FR-7): When several sessions are displaced from the same account under `spread`, the system shall move them to different accounts according to each session's own rendezvous order rather than to one shared head account.
- [ ] AC-012 (FR-7, FR-11): Where neither `state_path` nor `[store]` is configured, the system shall place a returning session that has never moved after restart on the same account it would have been placed on before, provided the pool and its health are unchanged; a session that had moved returns to its rendezvous home, an accepted exception to FR-4.
- [ ] AC-013 (FR-8): When the account with the soonest weekly reset recovers headroom under `soonest_weekly_reset`, the system shall consider it only when placing a new session or moving a displaced one, and shall leave every other binding in place.
- [ ] AC-014 (FR-9): When a session returns after more than 24 hours without a request, the system shall assign it as a new session.
- [ ] AC-015 (FR-9, FR-13): When the table holds 10,000 bindings and a new session arrives, the system shall evict the least recently used bindings in one batch down to about 90 % of the cap and increment the eviction count by the number removed.
- [ ] AC-016 (FR-10): When a Codex conversation's HTTP and WebSocket turns use the same session identity and the same configured provider, the system shall serve both from the same bound account.
- [ ] AC-017 (FR-11, FR-12): When the process restarts with `state_path` set, the system shall restore every unexpired binding whose account is still configured, and drop the rest.
- [ ] AC-018 (FR-12): When a hot reload removes a bound account, the system shall assign that account's sessions again on their next request with switch reason `failover`.
- [ ] AC-019 (FR-14): When two configured aliases resolve to one physical identity, the system shall report their bound sessions once, under that identity.
- [ ] AC-020 (FR-16): When `sort_by_reset` is enabled at runtime via the admin API, the system shall leave every existing binding in place.

## Out of Scope

- Config keys for the TTL or the size cap (fixed constants first; expose them later, once eviction counts show they are needed)
- A least-bound-sessions strategy
- Sharing bindings between differently named providers that use the same physical accounts
- Multi-instance shared bindings (ADR-0006 Phase 2)
- Changing the per-request failover order beyond the binding (`sort_by_reset`, burn-rate avoidance and the all-near guard stay as they are)
- Closing or merging #856 and #857 (a maintainer call made on those PRs)

## Assumptions

- The binding key keeps the selection key each path passes today, so placement keys do not change meaning: the Anthropic and outbound Responses/Codex paths pass the effective session id (not client-namespaced), and the inbound Codex endpoint and routed xAI pass `{client}:{session_id}` (the bare id without inbound auth). The delegated agent id stays in the WebSocket socket key only. Inbound HTTP and WebSocket turns of one conversation share one key because inbound WebSocket turns dispatch through the same passthrough selection.
- A needs-re-login account is not given a new exclusion rule. It moves a binding only by failing to serve, so another account serves successfully (FR-3).
- `spread` with no persistence keeps today's restart stability, but not today's exact placement: rendezvous hashing places sessions differently from `hash mod n`, so one reshuffle happens on upgrade. It happens with persistence too, because a pre-change state file and an existing store carry no bindings (NFR-3).
- 60 seconds of soft-near is an initial policy, not a measured optimum; the switch metric (FR-13) is how it will be tuned.
