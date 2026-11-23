# Plan: Rate-limit the workload token exchange endpoint

> Track: exchange-rate-limit-20261122
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: exchange-rate-limit-20261122
- **Issue**: #914
- **Created**: 2026-11-23
- **Approach**: Extend the existing fixed-window `PerIpRateLimiter`. It gains three entry points over one shared core:
  - a non-counting "blocked?" peek;
  - a failure recorder;
  - a counting identity admit.

  Two new instances live in `GatewayStores`, which is built at boot with or without
  `[server.gateway]`. The exchange handler checks the address before verification and the
  identity after it.
- **Plan Depth**: standard (risk guard: authentication endpoint)
- **Planned At**: 4d6b6c0

## Context

### Problem

The exchange endpoint has no rate limit (#914). The spec sets two limits. This section records
only what the executor needs to know about the code they land in.

### Current code

`POST /v1/oauth/token` is served by `src/auth/exchange/handler.rs::post`. Today it does four
things in order:

1. answers `404` when a reload removed `[server.auth.exchange]`;
2. checks the content type and parses the body;
3. tries `inbound_jwt::verify` for each opted-in rule;
4. mints on the first `Verified` result. Any `Unavailable` result gives `503`; otherwise it
   answers `401 invalid_grant`.

`[server.rate_limits]` (`src/config/http_tuning.rs`) holds the `/device` limits with a per-field
default merge. `src/config.rs` validates `max` and `window_seconds` as positive. `src/reload.rs`
already warns that any change needs a restart, and it compares the whole struct, so new fields
are covered for free.

`src/gateway/store.rs` is 649 lines, over the 500-line guideline. It holds `PerIpRateLimiter`,
which uses a fixed window, always increments on `check`, caps tracking at 4096 keys, and sweeps
idle entries.

The middleware OAuth reshaping (`src/error.rs::shape_gateway_error`) applies only to
middleware-made rejections. The handler's own responses reach the client as written.

### Non-Goals

The spec sets the scope: no replay prevention, no persistence, and no hot reload of limit values.

Considered and not built:

- **A separate limiter type for the exchange.** It would duplicate the sweep and capacity logic
  and work against the Sonar duplication gate. Reconsider if the shared core cannot express
  AC-013 without flags.
- **Counting identity-limit refusals as address failures.** That would let a busy but valid
  matrix workflow lock out its own runner address. Reconsider only if abuse shows valid callers
  cycling identities.

## Architecture Decision

**Limiter.** Refactor `PerIpRateLimiter::check_at` into a private core. The core does the sweep,
the capacity check, the window reset and the increment, and reports when the table is full.
`check` keeps its exact behaviour, so the `/device` limiters do not change. The new entry points
are:

- **peek** — answers "blocked, and for how long" without counting. An untracked key is never
  blocked, even when the table is full (AC-013).
- **record failure** — counts one failed attempt. When the table is full and the key is
  untracked, it reports a refusal without tracking the key (AC-013).
- **admit** — counts a success and refuses when the count is over `max`. A full table refuses
  new keys (AC-012).

Retry-After is the whole seconds left in that key's window, rounded up and clamped to
`[1, window_seconds]`.

**Placement.** Add `exchange_identity_rate` and `exchange_failure_rate` to `GatewayStores`. It is
built in `AppState::new` and `build_router` whether or not `[server.gateway]` exists, and it is
fixed at boot like the other limiters. A new `AppState` field was rejected: it would ripple
through `from_shared` and every test fixture.

**Keys.**

- The identity key is a digest of `(rule fingerprint, identity)`. The token is bound to its
  entry, so two entries on one issuer must not share a quota. The digest also bounds the size of
  the stored key.
- The address key is `device::client_ip` driven by `[server.access_control] trust_forwarded_for`
  from the refreshed config. It is truncated to a fixed small length so that a long forwarded
  header cannot defeat the memory bound.

**Handler order.**

1. `404` for a removed table.
2. Compute the address key, then peek. If the address is blocked, answer `429` before reading
   the body.
3. Content type, body and parse failures record a failure and keep their `400`.
4. All rules `Rejected` records a failure and keeps the `401`. `Unavailable` keeps the `503` and
   records nothing.
5. `Verified`: admit the identity before minting. A refusal answers `429`, mints nothing, and
   records no failure.
6. If recording a failure is itself refused (table full, key untracked), answer `429` instead.

Split `post` into small helpers as needed to keep functions short.

**429 contract.** Add an `ErrorCode` for status `429` with error `slow_down` (RFC 8628, "back
off") and one fixed description. Both limits use it, plus `Retry-After` and the existing
`no-store`. `temporarily_unavailable` was rejected because it already means the JWKS `503`.

## Tasks

- [x] T001 Move `PerIpRateLimiter` and its tests out of the gateway store into their own module, unchanged (file: src/gateway/rate_limit.rs)
  Validation: the device limiters behave exactly as before. `store.rs` drops below 600 lines, and every moved test passes unmodified.
  Method: `cargo test --all-features gateway::` plus `cargo clippy --all-targets --all-features -- -D warnings`
- [x] T002 Accept `[server.rate_limits] exchange` and `exchange_failure` with defaults and validation (file: src/config/http_tuning.rs) [FR-4, AC-009]
  Validation: an omitted table gives 60/600 and 10/600. A table that sets only `max` keeps the default window (AC-009). A zero `max` or `window_seconds` fails config load, naming the table. Changing either table on reload logs the existing restart warning.
  Method: unit tests in `src/config/http_tuning.rs` and `src/config.rs`
- [x] T003 [TDD] Add the peek, record-failure and admit entry points with Retry-After to the limiter (file: src/gateway/rate_limit.rs) (depends on T001) [NFR-1, AC-002, AC-007, AC-012, AC-013]
  Validation: at fixed instants, a key over `max` is blocked until its window elapses and then admitted (AC-002); Retry-After is always within `[1, window]` (AC-007); at 4096 keys a new identity is refused (AC-012) while an untracked address is not blocked and its failure is refused without being tracked (AC-013); `check` keeps its old behaviour.
  Method: `cargo test --all-features gateway::rate_limit` with injected `Instant`s
- [x] T004 Refuse over-limit addresses before verification and count failed exchanges (file: src/auth/exchange/handler.rs) (depends on T002, T003) [FR-2, FR-3, NFR-2, AC-004, AC-005, AC-006, AC-007, AC-014]
  Validation: after the configured number of failures from one address, its next request, even with a valid assertion, gets `429` with `error: slow_down`, `Retry-After` and `no-store`, and no verification runs (AC-004, AC-007); wrong content type, malformed or oversized body, unsupported grant type and an unverifiable assertion each count (AC-005), a success never counts (AC-006), and a `503` from an unavailable key set never counts (AC-014); a removed table still answers `404` even for a blocked address; requests through the real listener carry a peer address, so callers never share an `"unknown"` bucket.
  Method: `cargo test --all-features auth::exchange` plus a `tests/router_surface.rs` case that drives the real router to a `429` and checks status, headers and the OAuth body
- [x] T005 Cap successful exchanges per verified identity (file: src/auth/exchange/handler.rs) (depends on T004) [FR-1, AC-001, AC-002, AC-003, AC-008]
  Validation: once one identity reaches the configured maximum, its next valid assertion gets `429` and no token is minted (AC-001), and it is exchanged normally again after the window (AC-002); two identities from one address, or one identity under two rules, are counted separately (AC-003); an identity-limit `429` does not count as an address failure; its body and headers match the failure-limit `429` except for the `Retry-After` value (AC-008).
  Method: `cargo test --all-features auth::exchange`
- [x] T006 Derive the limiter's client address from `[server.access_control] trust_forwarded_for` (file: src/auth/exchange/handler.rs) (depends on T004) [FR-5, AC-010, AC-011]
  Validation: with the flag on, failures from one peer with different `X-Forwarded-For` values are counted per forwarded address (AC-010); with the flag off, the header is ignored and the peer address is counted (AC-011); an oversized forwarded value is stored as a bounded key.
  Method: `cargo test --all-features auth::exchange` with the `ConnectInfo` extension inserted, as `src/http_tuning.rs` tests do
- [x] T007 Document the exchange limits on every surface (file: site/src/content/docs/reference/configuration.md) (depends on T005) [FR-6]
  Validation: the configuration reference and the GitHub Actions OIDC guide (each en/ko/ja/zh-cn), `shunt.toml.example` and `docs/inbound-jwt-auth.md` describe the two tables, their defaults, the `429 slow_down` response with `Retry-After`, the use of `[server.access_control] trust_forwarded_for`, and that limit changes need a restart (FR-6); locale pages link only anchors that exist in that locale.
  Method: `grep -l exchange_failure` over the surfaces lists all ten files; `site` build succeeds and the built locale pages resolve their fragment links

## Key Files

- `src/auth/exchange/handler.rs`: modify. Peer extractor, limiter checks, the `slow_down` error code.
- `src/gateway/store.rs`: modify. Two limiter fields in `GatewayStores::new`; the test literal at `RateLimitsConfig { .. }` moves to `..Default::default()`.
- `src/gateway/rate_limit.rs`: new. The limiter and its tests, moved from `store.rs`.
- `src/config/http_tuning.rs`: modify. `RateLimitsConfig`, partial merge, defaults.
- `src/config.rs`: modify. The rate-limit validation loop.
- `src/gateway/device.rs`: reuse `client_ip`.
- `src/auth/exchange.rs`: reuse `fingerprint`.
- `src/auth/exchange/tests.rs`, `tests/router_surface.rs`: tests.
- `site/src/content/docs/{,ko/,ja/,zh-cn/}reference/configuration.md`, `site/src/content/docs/{,ko/,ja/,zh-cn/}guides/github-actions-oidc.mdx`, `shunt.toml.example`, `docs/inbound-jwt-auth.md`: docs.

## Verification

### Observable Outcomes

- Under both limits, every existing exchange response is unchanged (NFR-2).
- Over either limit, the caller gets `429 slow_down` with `Retry-After` and `no-store`.
- A blocked address costs no assertion verification.

### Automated Tests

- `cargo fmt --all --check`
- `PYO3_PYTHON=/usr/local/bin/python3 cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-features --workspace`
- Live check against a running instance with `exchange_failure.max = 2`: two bad assertions, then a third request gets `429 slow_down` with `Retry-After`, while `/v1/messages` is unaffected.

## Review Focus

- **Absent peer address** (no `ConnectInfo`, so the key falls back to `"unknown"`): every caller would share one failure bucket. Confirm the serving path always provides the peer address. Owned by T004.
- **Body-read failure** (oversized or aborted upload): counts as a failure and keeps its `400`, never a `500`. Owned by T004.
- **Window just elapsed**: a remaining time under a second still yields `Retry-After: 1`, never `0`. Owned by T003.
- **Oversized `X-Forwarded-For` or `X-Real-IP` value**: stored as a bounded key. Owned by T006.
- **Reload removes the table while an address is blocked**: answers `404`, not `429`. Owned by T004.

## Progress

- [x] (2026-11-23 KST) T001 Move `PerIpRateLimiter` and its tests out of the gateway store into their own module, unchanged
  Evidence: `cargo test --all-features gateway::` -> 357 passed, 0 failed (3 moved rate_limit tests ok); clippy --all-targets --all-features -D warnings exit 0; store.rs 566 lines
- [x] (2026-11-23 KST) T002 Accept `[server.rate_limits] exchange` and `exchange_failure` with defaults and validation
  Evidence: `cargo test --all-features http_tuning` -> 18 passed (defaults 60/600 and 10/600, partial-table, struct-differs tests); `config::tests` -> 343 passed incl. http_tuning_validation_rejects_each_invalid_value (zero exchange max, zero exchange_failure window); clippy --all-targets --all-features -D warnings exit 0
- [x] (2026-11-23 KST) T003 [TDD] Add the peek, record-failure and admit entry points with Retry-After to the limiter
  Evidence: RED `cargo test --all-features gateway::rate_limit` -> 7 failed, 3 passed (todo! stubs); GREEN -> 10 passed, 0 failed; `cargo test --all-features gateway::` -> 364 passed; clippy --all-targets --all-features -D warnings exit 0
- [x] (2026-11-23 KST) T004 Refuse over-limit addresses before verification and count failed exchanges
  Evidence: `cargo test --all-features --lib auth::exchange` -> 20 passed (6 new `rate_limit::*` handler tests: blocked valid assertion -> 429 with 0 JWKS fetches, each failure kind counts, success and 503 never count, 404 wins for a removed table, full table -> 429); `cargo test --all-features --test router_surface` -> 15 passed incl. the_exchange_path_answers_429_after_repeated_failures; clippy exit 0
  Also changed: src/auth/inbound_jwt.rs + inbound_jwt/tests.rs (test fixtures `pub(crate)` for reuse), tests/inbound/inbound_exchange_endpoint.rs (raise exchange_failure.max: its tests send >10 deliberate failures from one loopback address)
- [x] (2026-11-23 KST) T005 Cap successful exchanges per verified identity
  Evidence: `cargo test --all-features --lib auth::exchange` -> 24 passed (4 new: identity max -> 429 with no token and no address failure; window reset after a real 3.2s wait (3-second window, widened from 1s during review) at handler level; separate quotas per identity and per rule on one address; both 429s equal except Retry-After)
- [x] (2026-11-23 KST) T006 Derive the limiter's client address from `[server.access_control] trust_forwarded_for`
  Evidence: `cargo test --all-features --lib auth::exchange` -> 27 passed (3 new: trusted forwarded addresses counted per address and across peers; flag off ignores the header and counts the peer; 8 KiB forwarded value stored as a key of at most 64 bytes, distinct long values stay distinct)
- [x] (2026-11-23 KST) T007 Document the exchange limits on every surface
  Evidence: `cd site && bun run build` -> `225 page(s) built`, `bun run lint:docs` -> `134 file(s) lint clean`; ids serverrate_limits, serverauthjwt-optional, serverauthexchange-optional, workload-token-exchange present in site/dist; `grep -rn '#914\|not rate limited' docs site/src/content/docs shunt.toml.example README*.md` -> no matches
- [x] (2026-11-23 KST) Review-stage commits (SHA: `381268b7`, `fe80aac6`, `68690ffd`, `b39ccb11`, `395344bf`, `2ed77645`, `7fc0c60c`, `68ed7f15`)
- [x] (2026-11-23 KST) Pin the cancelled-request lesson with `a_cancelled_request_is_never_counted` (SHA: `7a7160df`)
  Evidence: `cargo test --all-features --lib auth::exchange::tests::rate_limit` -> 17 passed; a mutation that charges the attempt after `peek` with no refund fails it at the follow-up failure (`left: 429, right: 400`); `cargo test --all-features --workspace --no-fail-fast` -> 5003 passed, 2 failed (both in the untouched `tests/responses_chain_stream.rs`, environmental)

## Decision Log

- 2026-11-23: Client address comes from `[server.access_control] trust_forwarded_for`, not a new exchange key. This was the user's decision at spec review: one proxy flag avoids a split where every caller collapses into the proxy's address.
- 2026-11-23: At capacity, the failure table lets untracked addresses' valid exchanges through. The identity table refuses new identities. User's decision.
- 2026-11-23: The `429` error code is `slow_down`, and the identity key is scoped by rule fingerprint.
- 2026-11-23: `tests/inbound/inbound_exchange_endpoint.rs` raises `exchange_failure.max` to 10000 in its shared config. Its malformed-body test sends 12 deliberate failures from one loopback address and would otherwise hit the new default limit of 10; its assertions are unchanged and the limit has its own tests.
- 2026-11-23: Both limits share one neutral `slow_down` description ("Too many requests. Wait before retrying."), so the identity-limit `429` differs from the failure-limit `429` only in `Retry-After` (AC-008).
- 2026-11-23: The failure-limiter key is bounded in the exchange handler only: an address longer than 64 bytes is replaced by its base64url SHA-256 (43 chars), so distinct values stay distinct at a fixed size. `client_ip` and `/device` are unchanged. Test-only accessor `PerIpRateLimiter::tracked_keys` added to assert it.
- 2026-11-23 (review): The non-counting `peek` + later `record_failure` was replaced by `charge` (count the attempt and check the limit under one lock, before the body is read) + `refund` (on success, an identity-limit `429`, or an unavailable key set). With `peek`, concurrent requests from one blocked address could all reach verification. The user chose this at the review gate. The trusted forwarded value is also normalized to a bare IP (`ip:port` and `[v6]:port` share a key) through `gateway::device::parse_client_ip`, which `http_tuning` now reuses.
- 2026-11-23 (review, next round): The `charge`/`refund` change was reverted because it counted cancelled requests (a dropped handler future never refunds, so a JWKS outage counted client timeouts as failures, AC-014) and refused concurrent valid exchanges from one address (AC-006). `peek` + `record_failure` is kept and the one-burst bound (requests in flight when the address reaches its limit still complete; how many can be in flight is bounded only by `max_concurrent_requests`, and not at all when it is `0`) is documented. The forwarded-address normalization stays.

## Surprises & Discoveries

## Known Issues

Left open at the end of the review (2026-11-23), by the user's decision:

- **IPv6 address rotation.** The failure limit keys on the full IPv6 address, so a client that
  controls a `/64` can rotate addresses for a fresh failure budget, and can fill the 4,096-key
  table. Keying IPv6 by its `/64` prefix changes the address key and was left out of this track.
- **Identity window test holds the env lock.** `an_identity_is_exchanged_again_once_its_window_elapses`
  sleeps 3.2 seconds of real time while holding `test_env_lock()`, so other env-lock tests wait for
  it. The handler reads real `Instant`s, so the reset cannot be observed without the wait; the
  window was widened from 1 second to keep the in-window refusal stable under CI load, and the reset
  itself is also covered at limiter level with injected `Instant`s.
- **greptile did not run.** The local greptile CLI failed to start (broken Node install), so it is
  missing from every review iteration; gpt, ocr and cubic ran.

## Outcomes & Retrospective

### What Was Shipped

`POST /v1/oauth/token` enforces two limits that are on by default. `[server.rate_limits] exchange`
caps successful exchanges per verified identity, scoped by the rule that accepted the assertion (60
per 600 seconds by default). `exchange_failure` caps failed attempts per client address (10 per 600
seconds by default). An address over its failure limit is refused before the body is read or any
key set is fetched. Both limits answer `429 slow_down` with `Retry-After` and `no-store`. The
client address follows `[server.access_control] trust_forwarded_for`, and a forwarded `ip:port` is
keyed by its IP. `PerIpRateLimiter` moved out of the gateway store into `src/gateway/rate_limit.rs`
and gained the `peek`, `record_failure` and `admit` entry points. The limits are documented on
every surface.

### What Went Well

- Moving the limiter unchanged first (T001) kept the device-flow behaviour provably identical
  before the new entry points were added.
- Injected `Instant`s made the window, capacity and `Retry-After` rules testable at limiter level
  without waiting.
- The review loop caught real defects: a forwarded `ip:port` that gave one client a fresh budget
  per port, a failure that was logged twice, and doc claims that did not match the code.

### What Could Improve

- The review-stage switch from `peek` to `charge`/`refund` was reverted one round later because it
  broke AC-014 (a cancelled request was never refunded) and AC-006 (concurrent valid exchanges
  from one address were refused). Check a concurrency fix against every acceptance criterion
  before adopting it, not only against the race it closes. The cancelled-request case is now pinned
  by `a_cancelled_request_is_never_counted`.
- Seven review iterations ran, mostly on Minor findings. The engines kept re-raising IPv6 `/64`
  keying until the user's out-of-scope decision was written to the rejected-findings ledger.
  Record a scope decision in the ledger as soon as it is made.
- The greptile engine never ran because the local Node install was broken.

### Tech Debt Created

- The failure limit keys on the full IPv6 address, so a client that controls a `/64` can rotate
  addresses (TD-037).
- `an_identity_is_exchanged_again_once_its_window_elapses` holds `test_env_lock()` across a
  3.2-second real sleep (TD-038).

