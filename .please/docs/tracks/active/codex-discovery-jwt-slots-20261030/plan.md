# Plan: Codex model discovery accepts the gateway OAuth header and cookie

> Track: codex-discovery-jwt-slots-20261030
> Spec: [spec.md](./spec.md)

## Overview

- **Source**: /please:plan
- **Track**: codex-discovery-jwt-slots-20261030
- **Issue**: #869
- **Created**: 2026-10-30
- **Approach**: a third `gate::Slots` posture — `Client`'s static-token slots plus the `Codex` JWT slot scan — selected for Codex-shaped discovery responses; the `[server.gateway]` token is checked before a `503`
- **Plan Depth**: standard
- **Planned At**: ac87e35

## Purpose

After this change, a Codex CLI user with ChatGPT sign-in and gateway OAuth both on gets `200` from model discovery instead of `401`. They can verify it by watching for a `/models` request with no `failed to refresh available models` error.

## Context

### Problem

`src/discovery.rs::authentication_error` always calls `gate::authenticate(.., Slots::Client)`, and only `Slots::Codex` scans `jwt_header` / `jwt_cookie`. But `Slots::Codex` reads static tokens through `authenticate_bearer`, which skips `x-api-key`, so neither variant keeps discovery's current credentials while adding the slots. The handlers already carry a per-request `codex_shape` flag: `get_negotiated` computes it from `has_client_version_query`, and it exists only when `[server.codex_endpoint]` is on.

Separately, `authentication_error` returns `503` on `Outcome::Unavailable` before it checks the gateway login token. Today that is unreachable, because the bearer slot is the only JWT slot. Once the cookie or header is scanned, it makes AC-009 fail.

### Constraints

- `Client` and `Codex` keep their exact behaviour. Their other callers (`codex_endpoint.rs`, `codex_analytics.rs`, `usage.rs`, `oauth_usage.rs`, `proxy/failover.rs`) are not touched.
- `tests/inbound_codex_jwt_slots.rs` is 583 lines (TD-024), so new tests do not go there.

### Non-Goals

Spec Out of Scope applies unchanged; the real Codex catalog is a follow-up track.

Considered and not built:
- **`x-api-key` on `Slots::Codex`.** It would widen the inbound endpoint's posture, which is not requested. Evidence that would change this: Codex starting to send `x-api-key`.
- **A gate unit test for the new variant.** `src/auth/inbound_jwt/tests.rs` is 1713 lines, and T002's end-to-end tests reach every branch the variant adds.

## Architecture Decision

**Chosen.**
- Add a `Slots` variant that pairs `authenticate_client` with the JWT-slot scan. Replace `slots == Slots::Codex` with a predicate on `Slots`, so both postures that read the slots share one check.
- `authentication_error` picks the variant from the same `codex_shape` that picks the response shape, so slot posture and shape cannot disagree (FR-4).
- Evaluate the gateway token first, and answer `503` only when no gateway identity matched. For `Client` this changes nothing (NFR-1).

**Rejected.**
- **Discovery on `Slots::Codex`.** It drops `x-api-key` (FR-3).
- **A separate bool beside `Slots`.** It splits one posture across two parameters.
- **A discovery-specific gate function.** It would duplicate the slot order and `Unavailable` logic that FR-2 requires to match.

## Tasks

- [ ] T001 Move the shared Codex JWT test harness into `tests/common` (file: tests/common/codex_jwt.rs)
  Validation: `tests/inbound_codex_jwt_slots.rs` uses the shared gateway, config, and token helpers instead of defining them, and its tests pass with the same names, count, and assertions.
  Method: `cargo test --test inbound_codex_jwt_slots`, with the same pass count before and after
- [ ] T002 Codex-shaped discovery reads the `jwt_header` / `jwt_cookie` slots (file: src/auth/gate.rs) (depends on T001) [FR-1, FR-2, FR-3, FR-4, FR-5, NFR-1, NFR-2, AC-001, AC-002, AC-003, AC-004, AC-005, AC-006, AC-007, AC-008, AC-009]
  Validation: AC-001 through AC-009 pass end to end on `/models`, `/backend-api/codex/models`, and `/v1/models?client_version=…`. The slot posture follows exactly the `client_version` detection that picks the Codex shape, including a key with an empty value. A valid `x-api-key` static token wins over a repeated `jwt_header`. The doc comments and log text no longer call the slots inbound-Codex-endpoint-only.
  Method: new `tests/inbound_codex_jwt_discovery.rs` via `cargo test --test inbound_codex_jwt_discovery`, then `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all --check`
  STOP: the cookie `Origin` rule or repeated-slot handling needs request data beyond the discovery handler's `HeaderMap`
- [ ] T003 Document that Codex model discovery accepts the configured header and cookie (file: docs/inbound-jwt-auth.md) [FR-6]
  Validation: no page says the Codex discovery paths accept a JWT only in `Authorization: Bearer`. Each page that lists the routes reading `jwt_header` / `jwt_cookie` names the three Codex-shaped discovery variants and keeps the Anthropic `/v1/models` bearer-only, across `docs/inbound-jwt-auth.md`, `docs/m11-inbound-codex-endpoint.md`, and the site guides `connect-codex-cli.mdx` and `inbound-codex-endpoint.md` plus `reference/configuration.md`, in en/ko/ja/zh-cn. The site builds, and the edited pages' fragment links resolve in `site/dist`.
  Method: `rg -n "discovery"` over those files for stale wording; `cd site && bun run build` with an anchor check

## Key Files

- `src/auth/gate.rs` — `Slots`, `authenticate`
- `src/discovery.rs:136-200` — `authentication_error`
- `src/auth/jwt_slots.rs` — slot readers and `cookie_slot_allowed` (reused unchanged)
- `src/auth/slots.rs:44`, `src/auth/inbound.rs:111` — doc comments to update
- `tests/inbound_codex_jwt_slots.rs`, `tests/common/jwt.rs` — harness source for T001
- `docs/inbound-jwt-auth.md:283-294`, `docs/m11-inbound-codex-endpoint.md:105-119`

## Verification

### Observable Outcomes

- Each of AC-001 through AC-009 maps to a named test in `tests/inbound_codex_jwt_discovery.rs` and passes.
- `cargo test --workspace`, clippy with `-D warnings`, and `fmt --check` are green. When `--all-features` cannot run (no `ui/dist` or Python env), the default-feature run is recorded instead.
- `cd site && bun run build` succeeds with the edited pages in all four locales.

## Review Focus

- `/v1/models?client_version` (empty value) and `?foo=1&client_version=x` → slot posture equals the Codex-shape decision, with no second parse (T002)
- Valid `x-api-key` static token plus a repeated `jwt_header` → `200` (T002)

## Progress

## Decision Log

- 2026-10-30: The plan risk guard (inbound authentication path) promoted the plan depth from the requested `lightweight` to `standard`.
- 2026-10-30: Implementation stays on `amondnet/firefish`, the branch that holds the track commit, instead of an issue-linked branch off `main`, which would have left that commit behind. Requirement tags added to T002/T003 for the coverage gate.

## Surprises & Discoveries
</content>
</invoke>
