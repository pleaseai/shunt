# Plan: Workload token exchange for GitHub Agentic Workflows

> Track: wif-token-exchange-20261121
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: wif-token-exchange-20261121
- **Issue**: #913
- **Created**: 2026-11-21
- **Approach**: A new `src/auth/exchange` module owns the exchange token. It mints and verifies an HS256 token bound to a fingerprint of the verifying `[[server.auth.jwt]]` entry. The state lives on `InboundAuth`, so gate and hot reload pick it up; the strip matches the token's marker whether or not exchange is configured.
- **Plan Depth**: standard
- **Planned At**: 3ed99c0

## Context

### Problem

gh-aw keeps the credential in the AWF sidecar, so the only keyless path is the sidecar's Workload Identity Federation exchange, and shunt has no endpoint for it (spec Overview).

### Existing code

The AWF sidecar source (`github/gh-aw-firewall` `containers/api-proxy/anthropic-oidc-token-provider.js`) confirms the spec's request: a JSON body with FR-1's fields (`workspace_id` optional) and a 200 response read for `access_token` and `expires_in`. It requests the GitHub OIDC token with a configurable audience that defaults to `https://api.anthropic.com`.

shunt already verifies external JWTs. `inbound_jwt::verify` returns `Verified{identity, issuer}`, `Rejected`, or `Unavailable` (503). `gate::authenticate` is the one accept path for every route that takes a `[[server.auth.jwt]]` JWT: `/v1/messages`, count_tokens, discovery, `/usage`, `/api/oauth/usage`, the Codex endpoint, and Codex analytics. Admin, spend, and telemetry routes never call it, so AC-013 holds by construction once acceptance lives only in that gate.

`InboundAuth` is re-resolved in full on every hot reload (`RuntimeState::from_config`), and handlers re-snapshot it per request. Exchange state stored on `InboundAuth` therefore hot-applies with no reload code.

The credential-slot mirror invariant (`src/auth/slots.rs`) requires every accept predicate on `authorization` to have a by-value strip mirror in `inbound::consumed_by`. The trip-wire suite `src/auth/slots/tests.rs` enforces this through `credential_kinds()`, the `accepted_pairs` counts in `fixtures()`, and `accepted_by_any_gate`. That helper calls a fixed list of sync `InboundAuth` methods, so the exchange accept must be one and must be added to that list, or the strip ⊇ accept check passes without testing it.

`tests/router_surface.rs` inventories every route. A new route needs an `EXCHANGE_PATHS` entry, the all-surfaces config must enable it, and registering it with `.route(...)` directly in `src/server.rs` avoids the `COMPOSITION_COUNTS` merge gate. Its all-surfaces config already uses `0123456789abcdef0123456789abcdef` as the gateway secret, so the exchange secret there must differ (FR-12).

### Non-Goals

Spec Out of Scope applies. Rate limiting is filed as a follow-up issue when the track is published.

Considered and not built:

- Reusing `[server.gateway]`'s `aud = "shunt"` shape so the gateway strip covers the token. Rejected because that strip is active only when `[server.gateway]` is configured, and it blurs the two token families. Evidence that would change the call: none, since the user decided on separate families.
- A per-token revocation list. Per-entry and per-secret revocation (FR-8) needs no state. Evidence that would change the call: an operator needing to cut one run without touching its entry.

### STOP Conditions

- Accepting or rejecting the exchange token would require changing `gateway::jwt::verify` or the gateway session format, which is out of scope.

## Architecture Decision

**Token format.** HS256, signed with `[server.auth.exchange] signing_secret[0]` and verified with any listed secret. Claims: `iss` is a fixed shunt-owned exchange issuer string, `aud` differs from the gateway's `"shunt"`, `shunt_token_use = "workload-exchange"`, `sub` holds the identity, `iat`/`exp` hold the times, and `ent` holds the entry fingerprint. Neither family's verifier accepts the other: gateway `verify` requires `aud == "shunt"` and the gateway issuer, and the exchange verifier requires its marker. FR-12 forbids a secret shared with `[server.gateway]`, which also closes the HMAC path.

**Entry binding (FR-8).** The fingerprint is a SHA-256 over a canonical, field-ordered encoding of the entry's resolved verification rules: issuer, audience, algorithms, authorized parties, email rules, `require`, subject prefixes, and identity claim. It must not depend on `jwks_url`, key rotation, or process state. The encoding starts with a version tag, and it sorts the values inside each list (audience, algorithms, authorized parties, emails, domains, each `require` value list, subject prefixes), whose order carries no meaning. `clock_skew_seconds` and `max_token_age_seconds` are left out on purpose. They bound only the assertion's freshness at exchange time, so tuning them does not revoke tokens. A token verifies only when an `exchange = true` entry in the current snapshot has the token's fingerprint.

**HS256 primitives.** Factor the sign and constant-time verify helpers out of `src/gateway/jwt.rs` into one shared helper, rather than duplicating them. The repo's Sonar duplication gate is 3%.

**Accept.** A sync `InboundAuth::authenticate_exchange(headers) -> Option<String>` reads only the first `Authorization: Bearer` value. `gate::authenticate` calls it after the static-token check and before the JWT-entries early return, and returns `Authenticated{client: identity, static_token: false}`. This makes the spend principal the identity via the existing `failover.rs` composition (AC-004).

**Strip.** A new `ConsumedBy::ExchangeToken` in `inbound::consumed_by` matches by the unverified `shunt_token_use` marker alone. It runs regardless of whether `[server.auth.exchange]` is still configured, so tokens issued before the table was removed stay stripped (FR-11). It is the same fail-safe-by-shape reasoning as `has_shunt_shape`.

**Endpoint.** The handler is registered only when `[server.auth.exchange]` is present at boot. It answers 404 if a later reload removed the table, and `warn_on_restart_only_changes` warns when the table is toggled. It verifies the assertion by calling `inbound_jwt::verify` once per opted-in rule (`std::slice::from_ref(rule)`), in config order. The first `Verified` result wins, and the token is bound to that rule's fingerprint. If no rule verifies it and any call returned `Unavailable`, the response is 503. One call per rule is required because `JwtOutcome::Verified` carries only the issuer, which cannot distinguish two entries on the same issuer (AC-007). Verifying over opted-in rules only makes static tokens, gateway sessions, and earlier exchange tokens fail the same way as any foreign JWT (AC-008), with no special-casing. FR-12 also rejects an entry whose `issuer` equals the exchange issuer. Errors come from a new `{error, error_description}` builder with `Cache-Control: no-store`. This is a third error shape, recorded in AGENTS.md beside the Codex exception.

Alternatives rejected: an asymmetric signing key with a JWKS endpoint (more surface, and nothing outside shunt verifies the token); binding to the entry index (breaks on reorder, and does not revoke on a rule edit); accepting the token through `jwt_header`/`jwt_cookie` (the spec limits it to Bearer).

## Tasks

- [x] T001 Add `[server.auth.exchange]` and per-entry `exchange` config with load-time validation (file: src/config.rs) [FR-3, FR-12, AC-023]
  Validation: `shunt check` accepts a config with a valid exchange table and one opted-in entry. It fails with an entry-naming error for each FR-12 case: `exchange = true` without the table (AC-023), the table with no opted-in entry, an empty secret list, a secret under 32 bytes, `max_ttl_seconds = 0`, a secret equal to a `[server.gateway]` session secret, and an entry whose issuer equals the exchange issuer. Serializing the config never prints a secret.
  Method: `cargo test --all-features config::` plus a `cargo run -- check` against a sample TOML
- [x] T002 Mint and verify exchange tokens bound to the entry fingerprint (file: src/auth/exchange.rs) (depends on T001) [FR-5, FR-6, FR-8, AC-015, AC-016, AC-017, AC-018, AC-019, AC-020]
  Validation: a token verifies under any listed secret until `exp`. It is rejected after the verifying entry is removed, loses `exchange = true`, or has any verification rule changed, and after every secret that signed it is dropped (AC-015..AC-018, exercised through a real `shared.store` + `refreshed()` reload). It is not accepted by `gateway::jwt::verify`, and a gateway session token is not accepted by it (AC-019, AC-020).
  Method: `cargo test --all-features auth::exchange`
- [x] T003 Serve `POST /v1/oauth/token` (file: src/server.rs) (depends on T002) [FR-1, FR-2, FR-4, FR-9, FR-10, NFR-1, NFR-2, NFR-3, NFR-4, AC-001, AC-002, AC-005, AC-006, AC-007, AC-008, AC-009, AC-010, AC-011, AC-012, AC-024]
  Validation: the sidecar's exact JSON body yields 200 with `access_token`, `token_type: Bearer`, and `expires_in == max_ttl_seconds`, including for an assertion with under 60 s left (AC-001, AC-002, AC-012). Each rejection class returns its FR-10 code and status (AC-005..AC-011). The route is 404 when the table is absent at boot or removed by reload (AC-024). The response carries `Cache-Control: no-store`. A body without a JSON content type gets 400 `invalid_request`, and unknown fields are ignored. Every verification failure (AC-005..AC-008) returns a byte-identical `401 invalid_grant` body (NFR-3). No log line, metrics label, or `error_description` contains the assertion, the issued token, or which check failed (NFR-2).
  Method: new integration suite under `tests/inbound/` using `tests/common/jwt.rs` issuers, plus `cargo test --test router_surface`
- [x] T004 Accept exchange tokens in the inbound gate (file: src/auth/gate.rs) (depends on T002) [FR-7, AC-003, AC-004, AC-013, AC-014]
  Validation: an issued token as `Authorization: Bearer` with `anthropic-beta: oauth-2025-04-20` authenticates `/v1/messages`, count_tokens, `/v1/models`, `/usage`, and the Codex endpoint as the verifying entry's identity. Usage and spend are recorded under that identity (AC-003, AC-004). The same token in `x-api-key`, the `[server.auth]` header, `jwt_header`, or `jwt_cookie` does not authenticate (AC-014). Admin, spend API, and telemetry routes reject it (AC-013). A request carrying both a valid static token and an exchange token authenticates as the static client.
  Method: gate unit tests in `src/auth/inbound_jwt/tests.rs` style plus an integration test driving `/v1/messages` with `[server.spend]` metering on
- [x] T005 Strip exchange tokens at every forward site (file: src/auth/inbound.rs) (depends on T004) [FR-11, AC-021, AC-022]
  Validation: the exchange token is a `credential_kinds()` entry in `src/auth/slots/tests.rs`, and `accepted_by_any_gate` calls `InboundAuth::authenticate_exchange`, and the strip ⊇ accept suites pass with updated `accepted_pairs` counts. A valid, expired, rotated-out, or orphaned (table removed) token in `Authorization` never reaches the upstream mock on a passthrough route or a gated route (AC-021, AC-022), including after `[server.auth]` is removed entirely.
  Method: `cargo test --all-features auth::slots` plus an integration test asserting no credential reaches the upstream mock
- [ ] T006 Document the exchange across every docs surface (file: docs/inbound-jwt-auth.md) [FR-13]
  Validation: `docs/inbound-jwt-auth.md` has a gh-aw section with the issue's workflow frontmatter and the matching shunt config, and no longer calls itself verify-only. The site configuration, endpoints, and `guides/github-actions-oidc` pages and their ko/ja/zh-cn copies, README.md and its three translations, and AGENTS.md's error-shape rule describe the feature. The gh-aw example sets `oidc-audience` to a shunt-specific value and the entry's `audience` to the same value. The docs warn that the sidecar's default audience (`https://api.anthropic.com`) must not be used as the entry audience, and that a rule edit cuts a running workflow until the sidecar's next scheduled refresh. Each locale cross-page link resolves in the built site. The "shunt issues nothing" claims in code comments are corrected.
  Method: `cd site && bun run build` with a link check over `site/dist`, plus `grep -rn "issues nothing\|verify-only"` returning only intended hits

## Key Files

- `src/config.rs` — Modify: `InboundAuthConfig`, `InboundJwtConfig`, `resolve`, `Config::validate`, new `ConfigError` variants
- `src/config/session.rs` — Reuse: `deserialize_one_or_many_secrets` (widen visibility)
- `src/auth/exchange.rs` — Create: token mint/verify, fingerprint, endpoint handler
- `src/gateway/jwt.rs` — Modify: extract the shared HS256 helpers
- `src/auth/inbound.rs` — Modify: `InboundAuth` exchange state, `authenticate_exchange`, `ConsumedBy`, `consumed_by`
- `src/auth/gate.rs` — Modify: exchange accept branch
- `src/auth/slots.rs`, `src/auth/slots/tests.rs` — Modify: accept-site doc table, credential kinds, counts
- `src/server.rs` — Modify: conditional route registration
- `src/reload.rs` — Modify: restart-only warning arm
- `tests/router_surface.rs` — Modify: `EXCHANGE_PATHS`, all-surfaces config
- `tests/common/{jwt,codex_jwt}.rs`, `src/auth/inbound_jwt/tests/workload.rs` — Reuse fixtures; update struct literals
- `docs/inbound-jwt-auth.md`, `site/src/content/docs/{,ko/,ja/,zh-cn/}{reference/configuration.md,reference/endpoints.md,guides/github-actions-oidc.mdx}`, `README*.md`, `AGENTS.md` — Modify

## Verification

### Automated Tests

- [ ] `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features --workspace` (build `ui/dist` first)

### Manual Testing

- [ ] `cargo run -- check` against the gh-aw sample config in the docs
- [ ] Live run against a real IdP before merge: mint a GitHub Actions OIDC token in a workflow, exchange it at a running shunt, and call `/v1/messages` with the result. Full SC-1 (a gh-aw run longer than five minutes, including threat detection) is confirmed on a deployed instance after merge.

## Review Focus

- No `Content-Type: application/json`, or unknown extra fields → 400 `invalid_request` for the first, fields ignored for the second, never 415/422 (T003)
- Both a valid static token and an exchange token presented on one request → the static token wins and `static_token` stays true (T004)
- `[server.auth]` removed entirely while tokens are outstanding → the passthrough forward still strips them (T005)

## Test Scenarios

### T002

- Happy path: mint under config A, reload with config A unchanged (`require` keys in a different TOML order) → still verifies.
- Edge case: two entries on one issuer, token minted by entry 2, entry 1 edited → still verifies. Entry 2 edited → rejected.
- Error path: token with a valid signature but `shunt_token_use` absent, or `exp` in the past → rejected.

## Progress

- [x] (2026-11-21 KST) T001 config surface + validation — commit 5654cd2d
  Evidence: `cargo test --features bench --lib config::` → 484 passed, 0 failed (re-run by orchestrator); `shunt check` → `config ok` on the valid sample and exit 1 with the matching message for each of the 7 FR-12 variants (no table / no opt-in / empty secret / short secret / zero ttl / reserved issuer / gateway-shared secret).
  Deferred: `--all-features` run needs `ui/dist` (built before the 5.5 full-suite gate).
- [x] (2026-11-21 KST) T002 mint and verify exchange tokens bound to the entry fingerprint
  Evidence: `cargo test --features bench --lib auth::exchange` → 14 passed, 0 failed (reload via real `shared.store` + `refreshed()`: entry removed / opt-in cleared / `require` value dropped / each policy field edited revoke; freshness + `jwks_url` tuning does not; secret kept as non-first verifies, all signing secrets dropped rejects; same-secret gateway/exchange cross-rejection); `cargo test --features bench --lib gateway::jwt` → 16 passed (shared `src/auth/hs256.rs` helper, behaviour unchanged).
  Deferred: `--all-features` run (prefill-router needs Python); `--features bench` used locally.
- [x] (2026-11-21 KST) T003 serve `POST /v1/oauth/token`
  Evidence: `cargo test --features bench --test inbound inbound_exchange` -> 16 passed, 0 failed (sidecar body 200 + full `max_ttl`, 30 s assertion, no/odd federation ids, 8 failure causes give byte-identical 401, opt-in/shared-issuer, static + issued token as assertion, grant/body 400s, JWKS 500 -> 503, no table -> 404, reload removes table -> 404, `Cache-Control: no-store` on every answer); `cargo test --features bench --test router_surface` -> 14 passed (inventory + literal count 51); `cargo test --features bench --workspace --no-fail-fast` -> 4792 passed, 2 failed (both pre-existing `responses_chain_stream` environmental); clippy `-D warnings` clean.
  Deferred: `--all-features` run (prefill-router needs Python); `--features bench` used locally.
- [x] (2026-11-21 KST) T004 accept exchange tokens in the inbound gate
  Evidence: `cargo test --features bench --lib auth::gate::tests` -> 6 passed (bearer slot on all 3 postures; x-api-key / `[server.auth]` header / jwt_header / cookie / bare Authorization rejected; static wins; expired, orphan-entry, other-secret -> Rejected not Unavailable; opt-in cleared on reload and no exchange table -> Rejected); `cargo test --features bench --test inbound inbound_exchange_gate` -> 6 passed (real-endpoint token on /v1/messages, count_tokens, /v1/models, /usage, /v1/responses; spend metered under the identity and `bob` untouched; cap on the identity gives 429; static beside exchange bills `bob`; admin / spend API / `/v1/metrics` give the same 4xx as a junk bearer); mutation (gate branch forced to `None`) turned 3 integration + 1 unit test red; `cargo test --features bench --workspace --no-fail-fast` -> 4803 passed, 3 failed (all pre-existing `responses_chain_stream` environmental); clippy `-D warnings` and `fmt --check` clean.
- [x] (2026-11-21 KST) T005 strip exchange tokens at every forward site
  Evidence: `cargo test --all-features --lib auth::slots` -> 16 passed (exchange token is a `credential_kinds()` entry in 3 variants: verifies / expired / rotated-out secret; `authenticate_exchange` in `accepted_by_any_gate`; `accepted_pairs` 10->11 and 12->13, the one verifying token accepted in `Authorization: Bearer` only); `cargo test --all-features --test inbound inbound_exchange` -> 27 passed (new `inbound_exchange_strip`: valid token on a gated route, token on a deployment with no `[server.auth]`, rotated-out secret, orphaned by reload, expired via `max_ttl_seconds = 1` + 2.2 s wait; each asserts the mock got the request, no header carries the token, and the caller's `x-api-key` arrives); mutation (`ExchangeToken` return removed) turned 2 unit + 5 integration tests red; `cargo test --all-features --lib` -> 4004 passed; `--test inbound` -> 135 passed; clippy `--all-features -D warnings` and `fmt --check` clean.
  Deferred: `--all-features` run (prefill-router needs Python); `--features bench` used locally.

## Surprises & Discoveries

- 2026-11-21: `tests/responses_chain_stream.rs` fails locally without this track's code. `refused_port_is_deterministically_refused` and `a_body_error_before_message_stop_still_emits_the_terminal_error_event` fail on a pristine control worktree at the base commit 3ed99c0 (separate target dir), and `a_body_error_after_message_stop_ends_the_relay_silently` failed 5 of 6 runs there. They are environmental on this machine, not caused by the exchange.

## Decision Log

- 2026-11-21: T003 precedence when a request is wrong in several ways: non-object body or wrongly typed `grant_type`/`assertion` is `invalid_request`; then absent/other `grant_type` is `unsupported_grant_type`; then missing/blank `assertion` is `invalid_request`. The 400 body size cap is 64 KiB.
- 2026-11-21: separate signing key, table plus per-entry opt-in, and no rate limiting in this track (user decisions on the issue's open questions). A rule change revokes outstanding tokens (spec review D1).
- 2026-11-21: added `[FR/NFR/AC]` traceability tags to the task lines so the coverage gate maps them (no requirement change). Reused the issue-linked worktree branch as `code_branch` instead of creating one with `gh issue develop`.
