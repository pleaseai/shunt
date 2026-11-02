# Plan: Inbound JWT from a configured header or cookie for Codex gateway OAuth

> Track: inbound-jwt-header-cookie-20261030
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: inbound-jwt-header-cookie-20261030
- **Issue**: #860
- **Created**: 2026-10-30
- **Approach**: Two optional `[server.auth]` keys resolved into `InboundAuth`; the Codex gate (renamed `Slots::Codex`) scans static → bearer JWT → header JWT → cookie JWT through the unchanged `inbound_jwt::verify`; the configured header joins the single by-name strip point in `ShuntCredentials`
- **Plan Depth**: standard
- **Planned At**: 49110d8

## Purpose

After this change, an operator can point Codex CLI users at shunt with `gateway_oauth`, and each developer signs in with the organization's identity provider instead of sharing a static token. They can verify it works by completing a Codex turn whose spend and usage are recorded under the developer's identity.

## Context

### Problem

Every Codex Responses route (HTTP, web search via `serve`, WebSocket upgrade, per-turn re-auth) funnels through `codex_endpoint::authenticate_inbound` (`src/codex_endpoint.rs:333`); the analytics sink has its own copy (`src/codex_analytics.rs:66`). Both pass `Slots::Bearer`, which has no other caller, and `gate::authenticate` (`src/auth/gate.rs:57`) reads a JWT only from `bearer_token`.

### Constraints

- `InboundAuthConfig` (`src/config.rs:401`) is a struct literal at ~55 sites in ~31 files with no `..Default`.
- `InboundAuth` is shared with `AdminAuth`; new fields default to `None`.
- `ShuntCredentials::from_state` / `strip_reserved_slots` (`src/auth/slots.rs:214, 274`) is the one strip point every forward site calls; `cookie` is already in `RESERVED_SLOTS`. The WebSocket path captures header names at upgrade (`websocket.rs:99-114`) and re-strips per turn (`turn.rs:165-173`) so a mid-connection reload cannot leak a renamed header.

### Non-Goals

Everything under spec `## Out of Scope`. Considered and not built:

- Rewording the Codex 401 to name the new slots — it would disclose configuration to unauthenticated callers (NFR-2); revisit only on user confusion reports.
- Moving admin's `session_cookie` (`src/admin/mod.rs:527`) onto the new parser — changes admin behavior for no gain here.

### STOP Conditions

- The Codex CLI sends an `Origin` header on its own requests — FR-12 would block the intended client; stop and report.

## Architecture Decision

**Config.** `Option<String>` fields on `InboundAuthConfig`; a manual `Default` (reusing `default_auth_header()` / `default_tokens_env()` — `resolve()` still fails closed) lets every literal become `..Default::default()` in the same commit, so clippy `needless_update` never sees a half-converted literal and the next field costs nothing. Slot types and parsing live in a new `src/auth/jwt_slots.rs`, attached via `InboundAuth::with_jwt_slots`, so `config.rs` gains no logic. Validation runs inside `InboundAuthConfig::resolve()` after the JWT entries resolve (AC-011 then needs no env) and reuses `RESERVED_SLOTS`, so FR-8 and the strip list cannot drift; the `[server.admin] header` collision goes in `Config::validate`, which already resolves `AdminAuth` (`config.rs:4482`). One `ConfigError::InvalidInboundJwtSlot { key, message }` covers every clause. Reload needs nothing new: `resolve_inbound_auth` re-runs and `AppState::refreshed` snapshots per request.

**Gate.** Rename `Slots::Bearer` → `Slots::Codex` rather than add a variant beside it (that would leave one dead). Presented values are gathered with no I/O as absent / one token / repeated; verification runs lazily in FR-5 order and the first success wins. A repeated slot halts the scan with `Unavailable` if an earlier slot was unavailable, else `Rejected` — a blanket 401 on duplicates would contradict FR-9's 503. A slot is examined only when the scan reaches it, so a static or bearer credential that authenticates first wins even when a later slot is repeated. AC-004 needs no special case: `verify` rejects an unconfigured `iss` without fetching (`inbound_jwt.rs:150-155`). The cookie is scanned as bytes across every `cookie` field (HTTP/2 splits them). FR-12 reuses `same_origin_or_non_browser` (`websocket.rs:320`), moved to a shared spot; for the cookie slot only a missing `Origin` counts as non-browser — a present `Origin` that fails `to_str` (which the helper today treats as absent) disables the slot like an unparseable one. `Slots::Client` callers are untouched, which is what keeps `/v1/messages` and discovery bearer-only.

**Strip.** `ShuntCredentials` gains the configured header name, removed unconditionally — FR-8 forbids it from naming a shared slot, so no by-value check is needed — which covers pool passthrough, the routed allowlist, `/v1/messages` failover, and judge forwarding in one edit. The WebSocket path captures it as a third upgrade-time name. Strip lands before the gate reads the slot, so no commit authenticates a credential it would still forward.

**Test fixture.** Integration tests get their own small RSA/JWKS fixture in `tests/common/jwt.rs` (~30 duplicated lines) rather than a crate feature (CI plumbing) or a `#[path]` include (brittle); unit tests keep `src/auth/inbound_jwt/tests.rs`.

## Tasks

- [x] T001 Add `jwt_header` / `jwt_cookie` to `[server.auth]` with startup validation (file: src/config.rs) [FR-8, FR-10, AC-011, AC-012, AC-014, AC-015]
  Validation: each FR-8 clause fails startup with an error naming the key (AC-011, AC-012), including mixed-case or whitespace-padded reserved names and an empty string; with neither key set the resolved `InboundAuth` carries no header or cookie slot and every existing `config::` / `reload::` test passes unchanged (AC-015); a reload that adds `jwt_header` shows it on the next request snapshot (AC-014).
  Method: `cargo test --all-features --lib -- config:: reload::` and `cargo clippy --all-targets --all-features -- -D warnings`
- [x] T002 Strip the configured JWT header at every forward site (file: src/auth/slots.rs) (depends on T001) [FR-7, NFR-1]
  Validation: the configured header and every `cookie` field are absent after `strip_reserved_slots`; a WebSocket turn after a reload that removes or renames `jwt_header` still strips the upgrade-time name; with neither key set the forwarded header set is unchanged (NFR-1).
  Method: `cargo test --all-features --lib -- auth::slots codex_endpoint::websocket`
- [x] T003 Accept a JWT from the configured header on the Codex gate (file: src/auth/gate.rs) (depends on T002) [FR-1, FR-3, FR-4, FR-5, FR-9, NFR-2, AC-002, AC-006, AC-007, AC-013, AC-016]
  Validation: the header authenticates raw or with a `Bearer` scheme in any case (AC-002); a repeated header is `401` (AC-016), or `503` when an earlier slot's issuer was unreachable (AC-013); a wrong-audience token is `401` (AC-006); `Slots::Client` ignores the header (AC-007); the analytics sink accepts the same credential; the `401` body and the default-level log line are identical whichever slot was presented or failed, and no token value is logged (NFR-2).
  Method: `cargo test --all-features --lib -- auth::gate auth::jwt_slots codex_analytics`
- [x] T004 Accept a JWT from the configured cookie on the Codex gate (file: src/auth/jwt_slots.rs) (depends on T003) [FR-2, FR-12, AC-004, AC-005, AC-017]
  Validation: the cookie authenticates when split across several `cookie` fields and when quoted; a ChatGPT bearer plus a valid cookie authenticates (AC-004); the configured name repeated in one field or across fields is `401` (AC-005), and non-UTF-8 bytes neither hide a duplicate nor panic; an `Origin` that is cross-origin, `null`, or unparseable disables the cookie slot (AC-017) while a missing `Origin` keeps it.
  Method: `cargo test --all-features --lib -- auth::gate auth::jwt_slots`
- [x] T005 Prove header and cookie sign-in end to end on the Codex endpoint (file: tests/inbound_codex_jwt_slots.rs) (depends on T004) [FR-6, AC-001, AC-003, AC-008, AC-009, AC-010]
  Validation: against a mock JWKS and upstream — `/v1/responses` with only the header serves a turn (AC-001); a WebSocket upgrade on `/backend-api/codex/responses` with the cookie among others serves a turn, and a later turn after token expiry gets a `401` frame (AC-003); `/v1/messages` with only the header is `401` (AC-007); an over-cap cookie principal is refused with `usage_limit_reached` (AC-008); neither the pool nor a routed non-ChatGPT upstream receives the header or `cookie` (AC-009, AC-010); an unreachable issuer yields an OpenAI-shaped `503` (AC-013); a cross-origin WebSocket upgrade carrying only the cookie is `401` (AC-017).
  Method: `cargo test --all-features --test inbound_codex_jwt_slots`
- [x] T006 Document Codex gateway OAuth via `jwt_header` / `jwt_cookie` (file: site/src/content/docs/guides/connect-codex-cli.mdx) (depends on T004) [FR-11]
  Validation: `docs/inbound-jwt-auth.md` §1/§3, `shunt.toml.example`, the configuration reference, the Codex connection guide, and a pointer in the inbound Codex endpoint guide cover the keys, slot order, the Origin rule, and an Entra ID example with every FR-11 pitfall, the per-turn WebSocket re-auth on token expiry, and `max_request_header_bytes` / proxy cookie-size caps; the `ko`, `ja`, `zh-cn` copies match; no locale page links an anchor its built page lacks; the README and its translations are checked and either updated or recorded in the Decision Log as needing no change.
  Method: `cd site && bun run build` exits 0 and the section renders in `site/dist/{,ko/,ja/,zh-cn/}guides/connect-codex-cli/index.html`

## Key Files

- `src/config.rs` — `InboundAuthConfig` (:401), `resolve()` (:527), `ConfigError` (:2745), `Config::validate` (:4438, :4482)
- `src/auth/jwt_slots.rs` (new); `src/auth/inbound.rs` — `InboundAuth` (:33), `bearer_payload` (:165)
- `src/auth/gate.rs` — `Slots`, `authenticate` (:57); `src/auth/slots.rs` — `ShuntCredentials` (:199)
- `src/codex_endpoint.rs` (:333), `src/codex_analytics.rs` (:66), `src/codex_endpoint/websocket.rs` (:99, :320), `src/codex_endpoint/websocket/turn.rs` (:33, :107, :165)
- Fixtures: `src/auth/inbound_jwt/tests.rs`, `tests/common/mod.rs`, `tests/common/jwt.rs` (new)
- `tests/inbound_codex_jwt_slots.rs` (new)
- Docs: `docs/inbound-jwt-auth.md`, `shunt.toml.example`, `site/src/content/docs/{,ko/,ja/,zh-cn/}reference/configuration.md`, `site/src/content/docs/{,ko/,ja/,zh-cn/}guides/connect-codex-cli.mdx`, `site/src/content/docs/{,ko/,ja/,zh-cn/}guides/inbound-codex-endpoint.md`

## Verification

### Automated Tests

- [x] `cargo fmt --all --check`
- [x] `cargo clippy --all-targets --all-features -- -D warnings` (run with default features: `--all-features` needs `ui/dist` and a Python env this machine lacks; CI runs the full set)
- [x] `cargo test --all-features --workspace` (record, don't fix, pre-existing environment failures such as the `responses_chain_stream` refused-port trio) — run with default features for the same reason: 60 suites, 4447 passed, 0 failed

### Manual Testing

- [ ] A real Codex CLI with `gateway_oauth`, once with `delivery = { kind = "cookie", … }` and once with `{ kind = "header", … }`, against local shunt and an OIDC issuer, completes a turn and a follow-up — settling the spec assumption that Codex sends the token on upgrades, search, and analytics, and the FR-12 STOP condition.

### Acceptance Criteria Check

- [x] AC-001…AC-017 each map to the task named in its `Validation:` line and pass.

## Review Focus

- A `cookie` value with non-UTF-8 bytes must not hide a duplicate of the configured name or panic (T004).
- A configured `jwt_header` with surrounding whitespace or mixed case is normalized or rejected, never compared case-sensitively against the reserved names (T001).
- A header value `bearer <jwt>` (lowercase scheme) verifies the same token as `Bearer <jwt>` (T003).
- `Origin: null` or an unparseable `Origin` disables the cookie slot rather than counting as no `Origin` (T004).
- An expired IdP token on a cookie-authenticated WebSocket yields a `401` frame on the next turn, not an authorized turn (T005).

## Progress

- [x] (2026-10-30 KST) T001 Add `jwt_header` / `jwt_cookie` to `[server.auth]` with startup validation
  Evidence: `cargo +1.98.1 test --lib -- config:: reload::` -> 483 passed, 0 failed; `cargo +1.98.1 clippy --all-targets -- -D warnings` -> clean; `cargo +1.98.1 test --workspace` -> 59 suites, 4403 passed, 0 failed (default features; `--all-features` needs `ui/dist` and a Python env absent here)
- [x] (2026-10-30 KST) T002 Strip the configured JWT header at every forward site
  Evidence: `cargo +1.98.1 test --lib -- auth::slots codex_endpoint::websocket` -> 42 passed, 0 failed (no `--all-features`: `ui/dist` and a Python env are absent here); mutation check (disabling the upgrade-time `jwt_header` strip in `run_turn`) turned both new WebSocket tests red; `cargo +1.98.1 clippy --all-targets -- -D warnings` -> clean; `cargo +1.98.1 fmt --all --check` -> clean; `cargo +1.98.1 test --workspace` -> 59 suites, 4408 passed, 0 failed (one earlier full run reported 1 unidentified failure, three reruns were green)
- [x] (2026-10-30 KST) T003 Accept a JWT from the configured header on the Codex gate
  Evidence: `cargo +1.98.1 test --lib -- auth::gate auth::jwt_slots codex_analytics` -> 91 passed, 0 failed (no `--all-features`: `ui/dist` and a Python env are absent here); 11 new tests in `auth::inbound_jwt::tests` (header raw/any-case Bearer, fall-through, repeated, 503, audience, `Slots::Client`, non-UTF-8, byte-identical 401 body, analytics sink); `cargo +1.98.1 clippy --all-targets -- -D warnings` -> clean; `cargo +1.98.1 fmt --all --check` -> clean; `cargo +1.98.1 test --workspace` -> 4420 passed, 0 failed
- [x] (2026-10-30 KST) T004 Accept a JWT from the configured cookie on the Codex gate
  Evidence: `cargo +1.98.1 test --lib -- auth::gate auth::jwt_slots auth::inbound_jwt codex_endpoint::websocket auth::slots` -> 198 passed, 0 failed (no `--all-features`: `ui/dist` and a Python env are absent here); 9 parser/origin unit tests in `auth::jwt_slots::tests` and 9 gate tests in `auth::inbound_jwt::tests`; `cargo +1.98.1 clippy --all-targets -- -D warnings` -> clean; `cargo +1.98.1 fmt --all --check` -> clean; `cargo +1.98.1 test --workspace` -> all suites ok, 3591 lib tests passed, 0 failed
  Evidence: `cargo +1.98.1 test --lib -- auth::inbound_jwt auth::gate` -> 148 passed, 0 failed, including `a_repeated_header_stops_the_scan_before_a_valid_cookie`; mutation check: changing `Presented::Repeated => break` to `continue` in `verify_slots` turned that test red (FAILED), restored with `git checkout`, green again; clippy and fmt clean
- [x] (2026-10-30 KST) T005 Prove header and cookie sign-in end to end on the Codex endpoint
  Evidence: `cargo +1.98.1 test --test inbound_codex_jwt_slots` -> 8 passed, 0 failed, 3.9s (no `--all-features`: `ui/dist` and a Python env are absent here); clippy `--all-targets -D warnings` clean; `cargo +1.98.1 test --workspace` -> 60 suites, 4446 passed, 0 failed
- [x] (2026-10-30 KST) T006 Document Codex gateway OAuth via `jwt_header` / `jwt_cookie`
  Evidence: `cd site && bun run build` -> exit 0, 217 pages; new section present in dist/{,ko/,ja/,zh-cn/}guides/connect-codex-cli (7. gateway OAuth + Entra pitfalls) and `jwt_header` in all four configuration pages; every fragment link in the four edited locale page sets resolves to an id in the built HTML; `cargo fmt --all --check` clean. READMEs: one capability-row clause added to all four (no Decision Log entry needed). Also edited: docs/inbound-jwt-auth.md, shunt.toml.example, the 4 README files, inbound-codex-endpoint pointer x4
- [x] (2026-10-30 KST) Review-stage commits (SHA: `5f50908d`, `7c2333c9`, `43294b1e`, `db4b1b9c`, `0061570c`, `5c67df71`)

## Decision Log

- 2026-10-30: Added FR-12 / AC-017 (cookie slot requires no `Origin` or a same-origin one) during planning. Both the spec review and the architect flagged cross-site use of an ambient cookie: an operator can point `jwt_cookie` at a JWT cookie a browser already holds for the gateway's domain (e.g. Cloudflare Access `CF_Authorization`), and the existing Origin check runs only without `[server.auth]`. The Codex CLI is not a browser, so the guard does not narrow requested behavior.

- 2026-10-30: A repeated configured header or cookie rejects only when the scan reaches that slot; a static or bearer credential that authenticates first still wins (user decision at plan review). AC-005 and AC-016 were narrowed to "no earlier slot authenticates the caller", which keeps FR-9's `503` rule exceptionless; the repeated value is stripped before forwarding either way.

- 2026-10-30: Implementation started on `amondnet/crinoid` (this worktree's branch, linked to #860 with `gh issue develop --name`) rather than a fresh `gh issue develop --checkout` branch, which would have been cut from `main` without the unpushed track commits. The branch was rebased onto `main` (#859, a docs-only store-migration guide touching no track file), so `Planned At` moved from `03ececb` to its rebased equivalent `49110d8`.

- 2026-10-30 (review): A non-UTF-8 `Authorization` value no longer rejects outright on the Codex gate; it is `Unreadable` and the scan moves on to `jwt_header` and `jwt_cookie`, as FR-5's "first slot that succeeds" requires. `Slots::Client` and a Codex deployment with neither key set present only the bearer slot, so their outcome is unchanged (NFR-1).

- 2026-10-30 (review): `tests/inbound_codex_jwt_slots.rs` is 551 lines, over the AGENTS.md "preferably under 500" guideline. Kept as one file: it is eight end-to-end tests sharing one mock JWKS + upstream harness, and splitting it would duplicate that harness; neighbouring suites such as `src/auth/inbound_jwt/tests.rs` (1712 lines) are larger.

- 2026-10-30 (review): Extended the FR-8 `jwt_header` reject list to every header shunt reads or rewrites on the Codex endpoint. The ensemble review confirmed two consequences of the original list: `session-id` / `x-claude-code-session-id` are read by `extract_session_id` before the strip, so the IdP JWT became the session id and reached telemetry and synthesized upstream affinity headers (FR-7, NFR-2); and the WebSocket bridge rewrites `content-type`, `openai-beta`, and the hop-by-hop / `sec-websocket-*` headers before per-turn re-auth, so a JWT carried there authenticated the upgrade but no turn. Rejecting the names at startup keeps the fail-closed style of the rest of FR-8 and leaves session extraction and the bridge untouched; the key is new in this PR, so no released configuration changes.

## Surprises & Discoveries

- Observation: the FR-12 STOP condition (Codex CLI sending `Origin`) does not hold for current Codex. In `openai/codex` `main`, `ORIGIN` appears only in server-side middleware that rejects requests carrying it (`codex-rs/otel-trace-websocket`, `exec-server`, `app-server-transport`) and in tests that inject it to exercise that rejection; no client code sets it, and the client stacks (`reqwest`, `tokio-tungstenite` via `codex-rs/websocket-client/src/dialer.rs`) add none on their own.
  Evidence: grep.app search of `openai/codex` Rust sources for `ORIGIN` / `header::ORIGIN` / `connect_async` (2026-10-30). Source-level only; the plan's manual Codex CLI run still confirms it end to end.

## Outcomes & Retrospective

### What Was Shipped

Two optional `[server.auth]` keys, `jwt_header` and `jwt_cookie`, let the Codex CLI's `gateway_oauth` sign in to the inbound Codex endpoint. The Codex gate (`Slots::Codex`) scans static token → `Authorization: Bearer` JWT → header → cookie, verifies with the existing `[[server.auth.jwt]]` rules, stops at a repeated slot, and keeps the 401/503 split. The cookie is read only with no `Origin` or a same-origin one (FR-12). The configured header and every `cookie` field are stripped at the single forward strip point, and the WebSocket path re-strips the upgrade-time header name each turn. Startup rejects reserved names, the static and admin headers, and every header shunt reads or rewrites on the Codex path. Docs cover the keys, slot order, the Origin rule, and a Microsoft Entra ID walkthrough in English, `ko`, `ja`, and `zh-cn`.

### What Went Well

- The single strip point (`ShuntCredentials::from_state`) meant one edit covered pool passthrough, routed allowlists, failover, and judge forwarding.
- Mutation checks in the executor review (Origin rule disabled, header strip removed, repeated slot `break` → `continue`) proved the new tests fail on regression; the third one exposed a missing test, fixed before review.
- The FR-12 STOP condition was settled from `openai/codex` source instead of being left to the manual run.

### What Could Improve

- The FR-8 reject list was drafted from shunt's credential slots only. The ensemble review found that `jwt_header` could name a header shunt itself reads (`session-id`, leaking the JWT into the session id and upstream affinity headers) or rewrites on the WebSocket bridge. A reject list for an operator-named header should be derived from every place the request's headers are read, not only from the credential slots.
- Two of the four configured review engines never ran: greptile's CLI is not installed here, and cubic exited 1 on every run (disposal error, then a provider usage-policy rejection, then bare exit 1). Only gpt and ocr reviewed the diff.
- The real-time cookie-expiry test needed two rounds: the first widening still assumed the test's clock read and the claims' own read were within a second.
- `--all-features` cannot build in this environment (missing `ui/dist`, Python), so local verification ran on default features only; CI covers the rest.

### Tech Debt Created

- `CODEX_PATH_HEADERS` in `src/auth/jwt_slots.rs` is maintained by hand; a header the Codex endpoint or WebSocket bridge starts reading or rewriting later must be added there too (TD-023).
- `tests/inbound_codex_jwt_slots.rs` is 558 lines, over the 500-line guideline (TD-024).
