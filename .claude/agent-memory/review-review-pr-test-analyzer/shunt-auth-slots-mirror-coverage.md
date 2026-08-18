---
name: shunt-auth-slots-mirror-coverage
description: PR #391 src/auth/slots.rs + src/auth/slots/tests.rs mirror-invariant test coverage analysis — vacuity checks, fixture arithmetic verification, tripwire soundness.
metadata:
  type: project
---

PR #391 (issue #363) introduced `src/auth/slots.rs` (`ShuntCredentials`, `SHARED_SLOTS`,
`RESERVED_SLOTS`, `strip_reserved_slots`, `strip_consumed_slots`) as the single enumeration
point for the "mirror invariant": any value V an accept predicate would authenticate in slot S
must be stripped from S by every forward site. Reviewed with a deep adversarial pass (row-level
vacuity, hand-traced accepted_pairs arithmetic, tripwire self-matching, doc/code cross-checks) —
result was unusually clean; only minor/informational findings, no important/critical gaps.

**Verified sound (no finding):**
- `accepted_pairs: 10` (defaults) and `12` (collision) in `src/auth/slots/tests.rs` fixtures()
  are both arithmetically correct — hand-traced against real `InboundAuth`/`GatewayAuth`/
  `AdminAuth` predicate code, not re-derived from the same formula the test uses.
- `every_bulk_header_forward_is_a_registered_site` tripwire: constructs `.headers(` via
  `format!(".{}(", "headers")` specifically so the test file's own source doesn't contain the
  literal substring — confirmed no other `src/**/*.rs` file's comments/strings accidentally
  contain a literal `.headers(` either. The getter-exclusion (`.headers()` vs `.headers(`) is
  correct: grepped every `.headers(` occurrence in `src/` and only the 4 allowlisted files have
  genuine bulk-apply calls; all other hits are `X.headers()` getters.
- `src/discovery/upstream/tests.rs` diff (the only one of the 3 named pre-existing test files
  this PR touched) is a pure mechanical rename `InboundCredentialContext` → `ShuntCredentials`
  (verified via `git diff origin/main...HEAD`) — no assertion weakened. `src/proxy/failover/tests.rs`
  and `tests/inbound_codex_endpoint.rs` are untouched by this diff entirely.
- Candidate "missing credential kind" leads all resolved as non-issues on inspection:
  `GatewayAuth::authenticate_token` (bare value) is only ever used inside `consumed_by` (the
  strip predicate) and `authenticate_bearer` — no accept site ever calls it against a raw
  slot value directly, so no delivery shape needs to model it.
  `AdminAuth::authenticate_login_token` reads a POST form-body field, not a header slot, so it's
  legitimately outside the header-slot mirror invariant's scope.
  An expired-but-genuinely-minted gateway JWT (`consumed_by`'s shape-fallback via
  `is_shunt_shaped_token`) is already covered by a pre-existing, untouched test
  `expired_gateway_jwt_in_x_api_key_is_not_forwarded` in `discovery/upstream/tests.rs` — not a
  gap in this PR just because the new mirror table's `credential_kinds()` doesn't separately
  include it.
  `InboundAuth::authenticate_value`'s doc comment claims it's "shared by... the admin surface's
  login-form / token-header checks" but grep shows the admin surface actually calls a
  *different*, same-named method on `AdminAuth` (admin/mod.rs:175) — a real doc/code mismatch,
  but pre-existing (not touched by this diff's `+7` lines to inbound.rs), so out of scope to
  report on this PR.

**Findings actually reported (all minor, confidence 40-65):**
1. `ForwardSite::DiscoveryPassthrough` (wraps `discovery/upstream.rs::upstream_headers`,
   `AuthMode::Passthrough` branch) only ever copies `SHARED_SLOTS` (`authorization`,
   `x-api-key`) out of the inbound map before checking/stripping — so for the "defaults" fixture,
   every accepted (kind, shape) row using shape 4/5 (the configured `[server.auth]`/
   `[server.admin]` header, `x-shunt-token`/`x-shunt-admin-token` by default — NOT a shared slot)
   is vacuously "absent from output" at this site regardless of whether `strip_consumed_slots`
   works, because the site never even reads that header name into consideration. This turned out
   low-severity because the *same* by-value logic these rows would otherwise need to prove IS
   exercised non-vacuously by the "collision" fixture, where `[server.auth] header = authorization`
   and `[server.admin] header = x-api-key` alias those shapes onto shared slots. The two-fixture
   design is deliberate and covers this; still worth naming the exact vacuous rows for
   transparency since the task explicitly asked "which rows are trivially true."
2. `ForwardSite::InferenceFailover` always builds a single-route, all-passthrough,
   necessarily-same-origin chain, so it only ever exercises the same-origin passthrough branch of
   `headers_for_route` (the one calling `ShuntCredentials::strip_consumed_slots`) — never the
   credential-injecting branch (unconditional blanket `headers.remove("authorization")` +
   `.remove("x-api-key")`, `proxy/failover.rs:640-652`), the off-origin passthrough branch
   (`:619-627`), or `check_inbound_auth`'s actual 401 gate logic (`:530-564`). Low severity because
   those branches use unconditional/blanket stripping unrelated to the by-value mirror invariant
   under test, and are presumably covered elsewhere (untouched `proxy/failover/tests.rs`).
3. `contains_value` in the test helper does `String::from_utf8_lossy(bytes).contains(value)` —
   lossy UTF-8 decoding could theoretically mask a leak if a forward site ever emitted invalid
   UTF-8 bytes containing a mangled credential. Not exercised by current fixtures (all-ASCII
   tokens), purely theoretical, confidence 40.

Pattern to watch for on future PRs to this module: when a new forward site or credential kind
is added, check whether it only reads `SHARED_SLOTS` (vacuous for non-aliased custom header
configs, but that's fine — the collision fixture pattern is the correct way to force by-value
coverage) vs. actually needs `RESERVED_SLOTS`-by-name coverage too.
