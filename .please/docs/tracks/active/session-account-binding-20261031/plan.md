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
- [x] T004 Commit bindings and count switches on the Anthropic and Kimi paths (file: src/adapters/anthropic/mod.rs) (depends on T003) [FR-3, FR-13, AC-002, AC-003, AC-004, AC-005, AC-018]
  Validation: AC-002, AC-003, AC-004, AC-005 and AC-008 hold end to end, and a reload-removed account's sessions rebind with reason `failover` (AC-018). Every Anthropic and Kimi serve point commits, including the post-refresh Relay. A bound account that is attempted and fails over moves the binding with reason `failover`. `shunt.pool.session_account_switches{provider,reason,thinking_bound}` and the eviction counter are emitted with no account or session values. `thinking_bound` comes from one predicate in `src/accounts/session_binding.rs` over the request's adapter and upstream model: true for the Claude model families ADR-0008 names as losing thinking blocks on another account, false on Kimi, Responses/Codex and xAI, with a unit test per family. The integration tests that brute-force a SHA-256 slot (`tests/multi_account.rs`, `tests/kimi_multi_account.rs`) find a session for the account they target through a rendezvous helper, with their assertions unchanged.
  Method: `cargo test --all-features --test multi_account --test kimi_multi_account` plus a metrics unit test using the existing counter test hook
- [x] T005 Commit bindings on the Responses/Codex paths (file: src/adapters/responses/pool.rs) (depends on T004) [FR-10, AC-016]
  Validation: the streaming, HTTP-loop and WebSocket outbound paths, inbound passthrough and routed xAI each commit at their serve point, including the success after a credential refresh. A Codex conversation's HTTP and WebSocket turns are served from one bound account (AC-016). The WebSocket pool takes its account from selection and never places a conversation itself.
  Method: `cargo test --all-features --test codex_multi_account --test inbound_codex_endpoint` with the slot-search helper updated as in T004
- [x] T006 Persist bindings in the pool state file (file: src/state_persist.rs) (depends on T002) [FR-11, FR-12, AC-017, NFR-3]
  Validation: a restart with `state_path` restores unexpired bindings whose account is still configured and drops the rest (AC-017). A file without `bindings` loads with none. A file with `bindings` parses under a struct that lacks the field, which proves the previous release can read it (NFR-3). Persisted `last_used` advances at most hourly.
  Method: `cargo test --all-features state_persist`
- [x] T007 Persist bindings in the store (file: src/store/pool_session_repo.rs) (depends on T006) [FR-11, NFR-3]
  Validation: migration `0002` creates `pool_session_binding` with identical columns on SQLite and PostgreSQL. A flush writes only changed rows. Boot restores as the file path does. The one-time import carries a file's bindings. An undecodable row is dropped with a warning instead of failing boot. The existing store suites (dollar-free SQL, schema parity, import table list, schema-too-new hold) pass with the new migration included. Tests that hard-code schema version 1 or one applied migration (`src/store/open/tests.rs:63,72`, `src/store/hold/tests.rs:126,134,135`) read the expected value from `supported_version(embedded(..))` instead, and `migrator_with_extra` copies every real migration and adds `0003_probe.sql`, so each test keeps asserting the same property.
  Method: `cargo test --all-features store::` (PostgreSQL cases run where the contract harness finds a server)
  STOP: the schema-too-new hold test in `src/store/hold/tests.rs` can't be adapted without weakening what it asserts.
- [x] T008 Report bound sessions per account on the pool API (file: src/admin/mod.rs) (depends on T003) [FR-14, AC-019, AC-020]
  Validation: each account object in `GET /admin/api/pool` carries `bound_sessions`, counted once per physical identity across aliases (AC-019). The `PATCH` override of `sort_by_reset` leaves bindings in place (AC-020).
  Method: `cargo test --all-features admin::`
- [x] T009 Update root and engineering docs for bindings (file: docs/m8-anthropic-multi-account.md) (depends on T005, T007, T008) [NFR-4]
  Validation: the M8 restart-determinism promise (`:123`) is replaced by ADR-0008's wording. `docs/m10-codex-multi-account.md`, `m11`, `m9-admin-surface.md` (`:676` "no sticky flag"), `store.md` (including that the previous release refuses a store migrated to `0002`, so a rollback with `[store]` needs the store restored, unlike the state file), `running.md` and `dashboard-metrics.md` (metrics tables) are updated. So are `README.md` and its `ko`/`ja`/`zh-CN` copies, at the pool-controls row. ADR-0008's status moves to Accepted.
  Method: `rg -n "stable_session_index|deterministic across process restarts|tracks no sticky" docs README*.md` returns nothing stale
- [x] T010 Update the documentation site and its locales (file: site/src/content/docs/reference/configuration.md) (depends on T009) [NFR-4]
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

- [x] (2026-11-01 KST) T001 Add the `[server.pool] assignment` strategy key — 4a97a8bc
  Evidence: `cargo +1.98.1 test --features bench --lib pool_assignment` → 1 passed (absent→spread; spread/soonest_weekly_reset parse; "Spread"/"bogus"/"SOONEST_WEEKLY_RESET" rejected naming `assignment`); `shunt check` on `assignment = "Spread"` → error `unknown variant: found 'Spread' … for key "default.server.pool.assignment"`; examples updated.
- [x] (2026-11-01 KST) T002 Build the bounded session binding table — 8367b735
  Evidence: `cargo +1.98.1 test --features bench --lib accounts::session_binding` → 9 passed (raw id absent from export/Debug; odd keys incl. empty/10 MB hash; 24 h expiry at +1 s; cap evicts LRU in a ~90 % batch and counts evictions; older-generation commit → Stale). Generation 0 = unbound at selection; `#![allow(dead_code)]` left for T003 to remove.
- [x] (2026-11-02 KST) T003 Select through bindings and assignment strategies — 2fa7766b, a9a048a0
  Evidence: `cargo +1.98.1 test --features bench --lib accounts::` → 283 passed (11 `assignment::tests`, 23 `binding_selection_tests` covering AC-002, AC-005..AC-013, AC-018, AC-020, FR-15 and FR-16 tail order, every commit case, `forget_identity`); disabling the 60 s debounce failed both debounce tests (mutation check). Orchestrator re-run: fmt clean, clippy `--all-targets --features bench -D warnings` exit 0, `cargo +1.98.1 test --features bench --workspace` → 4647 passed, 0 failed. `account_pool_mixed_cycles`: an interleaved same-session A/B against 8367b735 measured 2fa7766b at +5.6 % to +10.6 % (over NFR-2's 5 %); the earlier "within 5 %" compared against a stale baseline. Perf follow-up a9a048a0 (skip the bindings lock while the table never held a binding; release the entries lock before placement when not re-probing; weight-only spread first choice; derive each account key once per selection) → orchestrator interleaved A/B (BASE, NEW, BASE, NEW; separate bench binaries) medians BASE 50.3/48.1, 141.5/141.0, 131.6/131.7, 140.0/134.9 ms vs NEW 39.0/38.4, 114.4/115.8, 111.4/111.7, 117.3/115.3 ms at 1/8/32/128 workers, about −21/−18/−15/−15 %. New benches `account_pool_binding_{hit,miss,eviction,concurrent_commit}`. CI `--all-features` remains the authority.
- [x] (2026-11-04 KST) T004 Commit bindings and count switches on the Anthropic and Kimi paths — 72ffa92d
  Evidence: Claude (:245) and Kimi (:892) select with a ticket; `SessionCommit` (`src/adapters/session_commit.rs`) commits on a 2xx at the first-attempt Relay, PauseSame retry, post-refresh Relay and Kimi Relay; a relayed 4xx/5xx commits nothing; `bound_failed_over` = the bound account was attempted after storm-control admission. `thinking_bound(adapter, auth, model)` is true only for Anthropic + `ClaudeOauth` + `claude-sonnet-5-5*`/`claude-haiku-5-5*` (6 family tests). Metrics `shunt.pool.session_account_switches{provider,reason,thinking_bound}` and `shunt.pool.session_binding_evictions{provider}`. Switch counts are asserted in-crate (`src/adapters/anthropic/binding_tests.rs`, wiremock through the real adapter loop) because the `#[cfg(test)]` getters are not visible to `tests/`; `tests/multi_account.rs` / `kimi_multi_account.rs` assert the binding through the ticket (AC-002, AC-003, AC-004, AC-005, AC-008 paused, AC-018 via a real reload, post-refresh serve). Mutation check: forcing `bound_attempted` false failed both failover tests. Orchestrator re-run: fmt clean, clippy exit 0, workspace → 4674 passed, 0 failed, 2 ignored.
- [x] (2026-11-04 KST) T005 Commit bindings on the Responses/Codex paths — a95ffd5f
  Evidence: outbound committed stream, HTTP loop, WebSocket, inbound passthrough and routed xAI select with a ticket and commit on a 2xx, including after a refresh; a relayed non-2xx and a re-probe-promoted serve commit nothing (`is_reprobe_for_account`, user-approved). `SessionCommit` is `&self` (AtomicBool) so the stream can share it; `admit_and_resolve` marks an admitted account attempted. The WebSocket pool keys sockets by the selected account and never places a conversation. Tests: `pool_binding_tests.rs` (switch counts, re-probe skip, WebSocket commit), routed-xAI serve point, `codex_multi_account` (+4), `inbound_codex_endpoint` (+3), `inbound_codex_websocket` AC-016 (HTTP then WebSocket on one `session-id` served by one account). One existing test changed input only (user-approved). Mutation checks: an always-false re-probe check and a removed WebSocket commit each failed their test. Orchestrator re-run: fmt clean, clippy exit 0, workspace → 4688 passed, 0 failed, 2 ignored.
- [x] (2026-11-04 KST) T006 Persist bindings in the pool state file — 2cd5f464
  Evidence: `PersistedPool.bindings` (optional, skipped when empty; `STATE_VERSION` stays 3) holds `{provider, session_hash hex, account, last_used}` rounded down to the hour; no raw session id. Restore resolves each named provider's pool and drops expired, unconfigured or malformed rows (FR-12), marking the pool dirty so the file is rewritten; `import_bindings` raises `any_bindings`. A same-hour touch does not dirty the pool. Hot-reload removal was already covered by T003/T004 (re-placed as `failover` on the next request). Tests in `src/state_persist/binding_tests.rs`: AC-017 restore/drop, first selection after restore follows the binding, v2/v3 without bindings load empty, a modelled previous-release struct reads a file with bindings and keeps quota (NFR-3), hourly `last_used`. Eight existing `PersistedPool` literals gained `bindings: Vec::new()` only. Mutation check: dropping the `any_bindings` raise failed both restore tests. Orchestrator re-run: fmt clean, clippy exit 0, workspace → 4694 passed, 0 failed, 2 ignored.
- [x] (2026-11-04 KST) T007 Persist bindings in the store — 6cdc2135
  Evidence: migration `0002_pool_session_binding.sql` (identical on SQLite and PostgreSQL apart from the sibling comment; `(provider, session_hash)` primary key, `last_used` index). `pool_session_repo` diffs the export against the last written snapshot, so a flush upserts only changed rows and deletes absent ones, in the same durable transaction as `write_pool`; boot shares the file path's `restore_bindings` filter; an undecodable row is dropped with a warning; the file→store import carries bindings. Approved test edits: `src/store/open/tests.rs` and `src/store/hold/tests.rs` read the version from `supported_version(embedded(..))`, `migrator_with_extra` copies every real migration plus `0003_probe`, and `tests/check_cli.rs` derives its expected version and pending count from `migrations/<engine>/*.sql` (user-approved); no assertion weakened. Orchestrator re-run: fmt clean, clippy exit 0 with `bench` and with `bench,store-postgres`, workspace → 4702 passed, 0 failed, 2 ignored; against a local PostgreSQL 18 (`SHUNT_TEST_POSTGRES_URL`) `store::`, `state_persist` and `check_cli` passed with the postgres arms running (93 postgres cases).
- [x] (2026-11-04 KST) T008 Report bound sessions per account on the pool API — c66114f8
  Evidence: every `GET /admin/api/pool` account row carries `bound_sessions`, the provider's live bindings on the row's physical identity; aliases of one identity share an `account_ref` and show the same single count (dedupe by `account_ref` to total), as the endpoint already does for request counters. One bindings-lock pass per request (`bound_counts`, skipped while `any_bindings` is false), taken outside `entries`; only counts are returned. `ui/src/types.ts` gained optional `bound_sessions`. Tests: `admin::pool_binding_tests` AC-019 (aliases report 3 once per identity, other provider excluded, no hash in the body) and AC-020 (`PATCH sort_by_reset` leaves `bound_sessions` and the export unchanged); per-provider expired-skipping count test. Orchestrator re-run: fmt clean, clippy exit 0, workspace → 4705 passed, 0 failed, 2 ignored.
- [x] (2026-11-04 KST) T009 Update root and engineering docs for bindings — 43d52cc4
  Evidence: M8 restart wording replaced by ADR-0008's, plus a `Session bindings` subsection (hashed key, shared table, `assignment`, move/no-move rules, 60 s debounce, 24 h TTL, 10,000 cap, persistence, metrics, `bound_sessions`); m9 `bound_sessions` alias rule; m10/m11 binding semantics; `store.md` migration 0002, `pool_session_binding` columns, rollback-needs-store-restore, `shunt check` v2 example; metric rows in `running.md` and `dashboard-metrics.md`; the four READMEs' pool rows in their own languages; ADR-0008 Accepted (and `decisions/index.md`); stale sticky wording fixed in seven further docs. `rg -n "stable_session_index|deterministic across process restarts|tracks no sticky" docs README*.md` → no matches; the 8 remaining `sticky`/`hash mod` hits describe the router, stage router, load balancers or historical plans. Orchestrator spot-check: rg empty, ko README row reviewed.
- [x] (2026-11-04 KST) T010 Update the documentation site and its locales — 1aed6f17
  Evidence: 13 pages in each of English, `ko`, `ja`, `zh-cn` (configuration `assignment` row, `sort_by_reset`, `state_path` bindings; endpoints `bound_sessions`; `shunt check` v2; anthropic guide `Session bindings` section; codex, pool-controls, store-migration rollback, OpenTelemetry metrics, inbound Codex, connect-codex-cli, comparison, anthropic provider, stage router); every page had a locale counterpart. `npm ci && npm run build` → 225 pages, no broken links; `npm run lint:docs` → 134 files clean; `npm run typecheck` → 0 errors. Locale anchors taken from `site/dist`: `session-bindings`, `세션-바인딩`, `セッションバインディング`, `会话绑定`. Orchestrator spot-check: ids present in each built locale guide, a ko cross-page link decodes to `#세션-바인딩`, ko CLI page shows `schema v2`.

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
- 2026-11-04: `AccountPool::any_bindings` (a9a048a0) lets selection skip the bindings lock while the table never held a binding. Every path that inserts a binding must raise it under the bindings lock, so T006's restore (`SessionBindings::import`) must set it when it imports any row.
- 2026-11-04: A serve by the re-probe-promoted account never creates a binding: on the Responses/Codex paths (the only ones that re-probe) the adapter skips the commit for it, so an unbound session stays unbound until a normal serve. This extends the plan's "no binding before: bind to the account that served" rule, which otherwise pinned a session to a stale near-quota account for the soft-near debounce (ADR-0008 §8). The user approved this.
- 2026-11-04: `adapters::responses::pool::tests::pool_path_bump_evicts_the_conversations_sockets_under_every_account` sends turn 1 sessionless (pool key kept), because a bound healthy account now keeps the session (FR-2). No assertion changed. The user approved this.
- 2026-11-04: The store schema is now v2, so the docs that show `shunt check` output as `schema v1 (binary supports v1)` (`docs/store.md` ~:220, `site/src/content/docs/reference/cli.mdx` ~:55 and its ko/ja/zh-cn copies) are bumped by T009/T010, not T007.
