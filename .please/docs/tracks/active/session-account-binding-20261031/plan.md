# Plan: Session-to-Account Bindings for Pooled Providers

> Track: session-account-binding-20261031
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: session-account-binding-20261031
- **Issue**: #873
- **Created**: 2026-10-31
- **Approach**: A pool-owned binding table, consulted inside `select_order_inner` and committed by each adapter at the point it knows which account served
- **Execution**: code
- **Plan Depth**: deep
- **Planned At**: a33fc90

## Context

### Problem

Displaced sessions move as a herd, return home on recovery, and no policy can choose their account (spec Overview; ADR-0008 Context).

### Where the change lands

The sticky slot is `start_slot` in `select_order_inner` (`src/accounts.rs:979-980`), with the fast path at `:1195-1204` and the shared fallback at `:1209-1252`. Selection returns only an ordered `Vec<usize>`, and only the adapter knows which account served:

- **Anthropic `claude_oauth`:** the Relay arm, `src/adapters/anthropic/mod.rs:360-392`; a `PauseSame` retry at `:478`; the post-refresh Relay at `:674-706`.
- **Kimi:** `:974`.
- **Outbound Responses:** `pool.rs:~755` (streaming), `:1175` (WebSocket), `:1305` (HTTP), and the post-refresh successes at `:918` (streaming) and `:1379-1397` (HTTP).
- **Inbound Codex:** `inbound.rs:~262`, and its post-refresh success at `:337`.
- **Routed xAI:** selection runs at `inbound_routed.rs:121` inside `routed_xai_pool_credential`, which returns only a `Credential`; the ticket and the chosen account go back to its caller (`:72`), which commits after its single send.

`AccountPool` lives for the whole process: a hot reload swaps `RuntimeState` and never touches it (`src/reload.rs:124`). A removed account shows up only as its absence from the `accounts` slice that selection receives.

Several existing pieces are reused rather than rebuilt:

- **Persistence:** `export_quotas`/`import_quotas` (`src/accounts.rs:2603-2663`), the v3 state file (`src/state_persist.rs:48-69`), and the store's whole-table `pool_quota` replace (`src/store/pool_repo.rs`).
- **Delta writes:** `src/store/sessions_repo.rs`, caller `src/gateway/durable.rs`.
- **Metrics:** `shunt.pool.rotations` and `record_pool_reprobe` (`src/metrics.rs:195,348,368`) are the template.

`account_pool_mixed_cycles` (`benches/perf_issues.rs:516`) uses a unique session per cycle through the public `select_order` wrapper, which discards the ticket and never commits, so every cycle takes the binding lock for a lookup miss and a placement without inserting; an extra mutex once cost it about 13 % (`src/accounts.rs:996-1000`).

### STOP Conditions

- Threading a binding ticket from selection to the serve point needs a change to a type or trait that `bench_support` or the stage router exports. Stop and report before changing that public surface.
- A required behavior can't be met without holding the binding lock across an `.await` (NFR-1).

### Non-Goals

The spec's Out of Scope list holds.

Considered and not built:
- **A file-mode shutdown flush:** none exists today; bindings share quota's 15 s loss window. Would change on evidence of bindings lost in normal restarts.
- **Client-namespacing the Anthropic key** (spec review: a client knowing another's session id could move its binding): session ids are random UUIDs, and namespacing changes today's key meaning. Would change for an untrusted multi-tenant deployment.
- **Config keys for TTL and cap.** Deferred until the eviction counter shows a need (ADR-0008 §4).

## Architecture Decision

The binding table is its own type in a new module, `src/accounts/session_binding.rs`, owned by `AccountPool` behind its own mutex, separate from `entries`. Each selection takes that lock once, briefly, before the entries lock, and never across an await. Lookup, placement and the decision to move all happen inside selection, where the health snapshot already exists.

Selection returns, next to the order, a small **binding ticket**: the session hash, the account bound before this request (if any), whether that account was ineligible for this request and why, and that binding's generation. The adapter passes the ticket back with the `AccountKey` that served and whether the bound account was attempted on this request and failed over (an upstream failure, a failed credential refresh or a transport error), through one `AccountPool` commit call at its success point. Only a relayed 2xx is a success point; a relayed 4xx commits nothing. The commit then does one of four things:

- **No binding before this request:** bind the session to the account that served. This is a placement, not a switch, so nothing is counted.
- **Same account served** (including a `PauseSame` retry on it): refresh `last_used`.
- **A different account served and the old one was ineligible, or was attempted and failed over:** move the binding, then record the switch with its reason (`failover` for an attempted-and-failed bound account).
- **A different account served for any other reason** (admission skip, re-probe promotion): change nothing.

The generation is per binding and every move bumps it. A commit whose ticket carries an older generation than the binding's current one loses (NFR-1); commits for other sessions never make a ticket stale.

Every adapter's failover loop stays as it is, and the sessionless and test wrappers keep their signatures.

**Placement:**
- `spread`: highest rendezvous weight, where the weight is `splitmix64(session_hash[..8] ^ account_digest)` and `account_digest` is the first 8 bytes of SHA-256 over the account key (Decision Log 2026-11-02), taken over eligible distinct identities, with priority ignored.
- `soonest_weekly_reset`: priority, then governing weekly reset (unknown last), then configuration order.

**On a move:** the same rule within the best available priority tier. The rest of the order, after the bound or placed account, keeps today's failover ordering, so `sort_by_reset` and headroom still apply (FR-16). An account that is ineligible for every candidate leaves today's order untouched and the binding unchanged.

**Hysteresis:** the soft-near start time lives on the account's health entry, separately for Fable and other requests, because the assessment differs between the two scopes. Any non-near assessment clears it.

**Persistence** follows the quota path in both backends (ADR-0008 §6): an additive field in the v3 file, and a delta repo modeled on `sessions_repo` for the store. A binding is a cache, unlike quota, so an unreadable row is dropped and boot continues.

Rejected:
- **Storing bindings inside `entries`:** it would put the binding table under the hot health lock.
- **Moving the binding in selection, before any serve:** a request that then fails everywhere would still move the session.
- **A v4 version bump:** the previous release would cold-start.

## Tasks

- [x] T001 Add the `[server.pool] assignment` strategy key (file: src/config.rs) [FR-6]
  Validation: `assignment = "soonest_weekly_reset"` and `"spread"` parse, absent means `spread`, and any other value makes `shunt check` fail naming the key; `shunt.toml.example` and `shunt.yaml.example` show the key.
  Method: `cargo test --all-features config::` plus `cargo run -- check` on a config with an invalid value
- [x] T002 Build the bounded session binding table (file: src/accounts/session_binding.rs) [FR-1, FR-9, AC-001, AC-014, AC-015, NFR-1]
  Validation: keys are (provider, SHA-256 of the selection key), and a raw id never appears in the table or its export (AC-001); an entry idle 24 h is gone (AC-014); at 10,000 entries a new insert evicts the least recently used and bumps an eviction count, at amortized constant cost (AC-015); a commit carrying an older generation does not overwrite a newer binding (NFR-1).
  Method: `cargo test --all-features accounts::session_binding`
- [x] T003 Select through bindings and assignment strategies (file: src/accounts.rs) (depends on T001, T002) [FR-2, FR-4, FR-5, FR-7, FR-8, FR-12, FR-15, FR-16, AC-006, AC-007, AC-008, AC-009, AC-010, AC-011, AC-012, AC-013, AC-018, NFR-2]
  Validation: AC-006 to AC-013 and AC-020 hold at the selection level; a binding whose account is absent from the `accounts` slice is placed again as if unbound, with the ticket carrying the removed account as ineligible (reason `failover`) so the commit moves the binding and records the switch (AC-018), and `forget_identity` drops its bindings; sessionless and empty-id requests still round-robin (FR-15); `account_pool_mixed_cycles` stays within 5 % of its `main` result.
  Method: `cargo test --all-features accounts::` plus `cargo bench --features bench --bench perf_issues -- account_pool`, with new bench cases for binding hit, miss, eviction and concurrent commit, driven through `pub` ticket-returning selection and commit methods on `AccountPool`, which `benches/perf_issues.rs` reaches as `shunt::accounts::AccountPool` (not through `bench_support`)
  STOP: an existing selection test fails for a reason other than the sticky slot formula, meaning a behavior the spec keeps has changed.
- [ ] T004 Commit bindings and count switches on the Anthropic and Kimi paths (file: src/adapters/anthropic/mod.rs) (depends on T003) [FR-3, FR-13, AC-002, AC-003, AC-004, AC-005, AC-018]
  Validation: AC-002, AC-003, AC-004, AC-005 and AC-008 hold end to end, and a reload-removed account's sessions rebind with reason `failover` (AC-018). Every Anthropic and Kimi serve point commits, including the post-refresh Relay. A bound account that is attempted and fails over moves the binding with reason `failover`. `shunt.pool.session_account_switches{provider,reason,thinking_bound}` and the eviction counter are emitted with no account or session values. `thinking_bound` comes from one predicate in `src/accounts/session_binding.rs` over the request's adapter and upstream model: true for the Claude model families ADR-0008 names as losing thinking blocks on another account, false on Kimi, Responses/Codex and xAI, with a unit test per family. The integration tests that brute-force a SHA-256 slot (`tests/multi_account.rs`, `tests/kimi_multi_account.rs`) find a session for the account they target through a rendezvous helper, with their assertions unchanged.
  Method: `cargo test --all-features --test multi_account --test kimi_multi_account` plus a metrics unit test using the existing counter test hook
- [ ] T005 Commit bindings on the Responses/Codex paths (file: src/adapters/responses/pool.rs) (depends on T004) [FR-10, AC-016]
  Validation: the streaming, HTTP-loop and WebSocket outbound paths, inbound passthrough and routed xAI each commit at their serve point, including the success after a credential refresh. A Codex conversation's HTTP and WebSocket turns are served from one bound account (AC-016). The WebSocket pool takes its account from selection and never places a conversation itself.
  Method: `cargo test --all-features --test codex_multi_account --test inbound_codex_endpoint` with the slot-search helper updated as in T004
- [ ] T006 Persist bindings in the pool state file (file: src/state_persist.rs) (depends on T002) [FR-11, FR-12, AC-017, NFR-3]
  Validation: a restart with `state_path` restores unexpired bindings whose account is still configured and drops the rest (AC-017). A file without `bindings` loads with none. A file with `bindings` parses under a struct that lacks the field, which proves the previous release can read it (NFR-3). Persisted `last_used` advances at most hourly.
  Method: `cargo test --all-features state_persist`
- [ ] T007 Persist bindings in the store (file: src/store/pool_session_repo.rs) (depends on T006) [FR-11, NFR-3]
  Validation: migration `0002` creates `pool_session_binding` with identical columns on SQLite and PostgreSQL. A flush writes only changed rows. Boot restores as the file path does. The one-time import carries a file's bindings. An undecodable row is dropped with a warning instead of failing boot. The existing store suites (dollar-free SQL, schema parity, import table list, schema-too-new hold) pass with the new migration included. Tests that hard-code schema version 1 or one applied migration (`src/store/open/tests.rs:63,72`, `src/store/hold/tests.rs:126,134,135`) read the expected value from `supported_version(embedded(..))` instead, and `migrator_with_extra` copies every real migration and adds `0003_probe.sql`, so each test keeps asserting the same property.
  Method: `cargo test --all-features store::` (PostgreSQL cases run where the contract harness finds a server)
  STOP: the schema-too-new hold test in `src/store/hold/tests.rs` can't be adapted without weakening what it asserts.
- [ ] T008 Report bound sessions per account on the pool API (file: src/admin/mod.rs) (depends on T003) [FR-14, AC-019, AC-020]
  Validation: each account object in `GET /admin/api/pool` carries `bound_sessions`, counted once per physical identity across aliases (AC-019). The `PATCH` override of `sort_by_reset` leaves bindings in place (AC-020).
  Method: `cargo test --all-features admin::`
- [ ] T009 Update root and engineering docs for bindings (file: docs/m8-anthropic-multi-account.md) (depends on T005, T007, T008) [NFR-4]
  Validation: the M8 restart-determinism promise (`:123`) is replaced by ADR-0008's wording. `docs/m10-codex-multi-account.md`, `m11`, `m9-admin-surface.md` (`:676` "no sticky flag"), `store.md` (including that the previous release refuses a store migrated to `0002`, so a rollback with `[store]` needs the store restored, unlike the state file), `running.md` and `dashboard-metrics.md` (metrics tables) are updated. So are `README.md` and its `ko`/`ja`/`zh-CN` copies, at the pool-controls row. ADR-0008's status moves to Accepted.
  Method: `rg -n "stable_session_index|deterministic across process restarts|tracks no sticky" docs README*.md` returns nothing stale
- [ ] T010 Update the documentation site and its locales (file: site/src/content/docs/reference/configuration.md) (depends on T009) [NFR-4]
  Validation: the configuration reference (the `assignment` row and session stickiness), the endpoints reference, and the multi-account, pool-controls, store-migration and OpenTelemetry guides describe bindings in English, `ko`, `ja` and `zh-cn`. Every locale cross-link resolves against the built locale page.
  Method: site build (its documented build command) with no broken-link errors, and `rg` of `site/dist/<locale>` for every new fragment link

## Key Files

- `src/accounts.rs`: `select_order_inner`, `forget_identity`, quota export/import, selection tests
- `src/accounts/session_binding.rs` (new), `src/config.rs` (`PoolConfig`), `src/metrics.rs`
- `src/adapters/anthropic/mod.rs`, `src/adapters/responses/{pool,inbound,inbound_routed}.rs`: serve points
- `src/state_persist.rs`, `src/state_persist/store_persister.rs`, `src/store/{pool_session_repo,import}.rs`, `migrations/{sqlite,postgres}/0002_pool_session_binding.sql`
- `src/admin/mod.rs`, `benches/perf_issues.rs`, `tests/{multi_account,kimi_multi_account,codex_multi_account,inbound_codex_endpoint}.rs`

## Verification

### Automated Tests

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `RUSTFLAGS=-D warnings cargo test --all-features --workspace`
- [ ] `cargo bench --features bench --bench perf_issues -- account_pool` compared with `main`
- [ ] `cargo bench --features bench --bench stage_router` compared with `main` (NFR-2)

### Observable Outcomes

- With a pool of 3 accounts and `state_path` set, `GET /admin/api/pool` shows `bound_sessions` per account, and the counts survive a restart.
- With OpenTelemetry export on, a forced cooldown emits `shunt.pool.session_account_switches` with `provider`, `reason` and `thinking_bound` attributes, and no session or account value in any attribute.

### Acceptance Criteria Check

- [ ] Every AC in spec.md maps to a passing test named in the task that owns it

## Review Focus

- Session id header values that are empty, non-UTF-8 or very long must hash without panicking, and empty ones stay sessionless (T002, T003).
- A state file whose `bindings` name an unknown account, a malformed hash or a future `last_used` drops those rows and keeps the rest (T006).
- `assignment` values differing only in case (`"Spread"`) fail `shunt check` instead of silently falling back to the default (T001).

## Test Scenarios

### T003

- Edge case: bound account soft-near for 59 s, then a non-near header, then soft-near again → no move until 60 s more of continuous soft-near (AC-006, AC-007).
- Edge case: a re-probe promotes C to index 0 for a session bound to A, and C serves → the binding stays on A.

## Progress

- [x] (2026-11-01 KST) T001 Add the `[server.pool] assignment` strategy key — 262b23e9
  Evidence: `cargo +1.98.1 test --features bench --lib pool_assignment` → 1 passed (absent→spread; spread/soonest_weekly_reset parse; "Spread"/"bogus"/"SOONEST_WEEKLY_RESET" rejected naming `assignment`); `shunt check` on `assignment = "Spread"` → error `unknown variant: found 'Spread' … for key "default.server.pool.assignment"`; examples updated.
- [x] (2026-11-01 KST) T002 Build the bounded session binding table — e6c9cf00
  Evidence: `cargo +1.98.1 test --features bench --lib accounts::session_binding` → 9 passed (raw id absent from export/Debug; odd keys incl. empty/10 MB hash; 24 h expiry at +1 s; cap evicts LRU in a ~90 % batch and counts evictions; older-generation commit → Stale). Generation 0 = unbound at selection; `#![allow(dead_code)]` left for T003 to remove.
- [x] (2026-11-02 KST) T003 Select through bindings and assignment strategies — ee6ff2fd
  Evidence: `cargo +1.98.1 test --features bench --lib accounts::` → 283 passed (11 `assignment::tests`, 23 `binding_selection_tests` covering AC-002, AC-005..AC-013, AC-018, AC-020, FR-15 and FR-16 tail order, every commit case, `forget_identity`); disabling the 60 s debounce failed both debounce tests (mutation check). Orchestrator re-run: fmt clean, clippy `--all-targets --features bench -D warnings` exit 0, `cargo +1.98.1 test --features bench --workspace` → 4647 passed, 0 failed. `account_pool_mixed_cycles` vs a fresh same-session baseline (47.7/146.0/137.6/140.0 ms at 1/8/32/128 workers): +1.5/+1.8/−0.2/+1.0 % and +2.3/+4.7/+3.3/+2.7 % on two runs, within 5 %. New benches `account_pool_binding_{hit,miss,eviction,concurrent_commit}`. CI `--all-features` remains the authority.

## Decision Log

- 2026-10-31: State file stays v3 with an optional `bindings` field (no v4 bump), so a file-mode downgrade keeps quota (a store-mode downgrade still meets `SchemaTooNew`); ADR-0008 §6 amended.
- 2026-10-31: Binding key = each path's existing selection key (spec assumption corrected).
- 2026-10-31: Persisted `last_used` is coarsened to one hour.
- 2026-10-31: `spread` ignores priority on first placement; a move respects priority tiers.
- 2026-10-31: Rebased onto `main` at `a33fc90` (#846, #871) before implementation and re-verified the plan against it. Line references in `src/accounts.rs` shift down by about 38. #871's `ChatgptPlanOauth` pools serve through the same `pool.rs` success arms (via `pool_plan::relay_events` / `peek_if_plan`), so T005 covers them. ADR renumbered to 0008 because #871 took 0007.
- 2026-11-01: The integration-test slot-search helpers (`tests/multi_account.rs`, `tests/kimi_multi_account.rs`, `tests/codex_multi_account.rs`, `tests/inbound_codex_endpoint.rs`) move from T004/T005 into T003, because T003 changes placement and the suite must stay green at every commit.
- 2026-11-02: The rendezvous weight is `splitmix64(session_hash[..8] ^ account_digest)`, where `account_digest` is the first 8 bytes of SHA-256 over the account key, cached per health entry. The SHA-256-per-pair weight from T002 cost +35 to +42 % on `account_pool_mixed_cycles`. It is still deterministic across restarts.
- 2026-11-02: `SessionBindings` nests by provider, so a lookup does not allocate.
- 2026-11-02: When the head is the session's first choice and healthy, the order is today's rotation from the head. Otherwise it is today's sorted failover order with the head moved to the front. `sort_by_reset` never decides a head.
- 2026-11-02: An ineligible bound account with no healthy alternative leaves the binding (AC-004). The exception is a bound account that left the slice, whose ticket always carries `failover` (AC-018).
- 2026-11-02: Seven existing selection tests (burn-rate headroom, hard-threshold backstop, paused-sticky ranking, the three `sort_by_reset_*` tests, priority ordering) now observe their sort on a sessionless request, with assertions unchanged. A session's head is now its rendezvous placement (FR-7, FR-16). The user approved this.
- 2026-11-02: The integration tests share `tests/common/mod.rs::session_id_for_account`, which searches with a fresh `AccountPool` over the gateway's resolved pool accounts.
