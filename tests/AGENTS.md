# tests Agent Instructions

## Build & Run Commands

- Run all tests from repository root: `cargo test --all-features --workspace`
- Run integration tests from repository root: `cargo test --test passthrough` and `cargo test --test responses_translate`
- Run one grouped binary: `cargo test --test spend`. Run one file inside it by
  its module path: `cargo test --test spend spend_headers::`.
- Run quality gates from repository root: `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D warnings`

## Testing

- `passthrough.rs` uses Wiremock and a real Axum server to verify gateway HTTP behavior.
- `responses_translate.rs` verifies Anthropic Messages → OpenAI Responses request conversion and Responses SSE → Anthropic SSE conversion.
- Prefer behavior-focused assertions over implementation-only assertions.
- A test that touches the process environment takes the shared guard from
  `tests/common/mod.rs` — `common::set_env(..).await` in an async test,
  `common::set_env_blocking(..)` in a sync one — and holds it for the whole
  body. Never call `std::env::set_var` or `std::env::remove_var` directly.
  `setenv` may reallocate the single global `environ` array, so a write to any
  variable can be observed by a concurrent `getenv` for an *unrelated* one as a
  missing value; unique variable names do not help, and neither does locking
  only the writers. That last point is why a test that only *reads* the
  environment — including indirectly, by building a config or resolving a
  workspace — takes the guard too. `tests/env_lock_coverage.rs` enforces the
  ban on raw writes: it scans for the `set_var`/`remove_var` spellings, so a
  reader that skips the guard is invisible to it. That half of the rule is held
  up by review, not by the gate.
- `tests/antigravity_process.rs` and `tests/env_guard.rs` hold the only reviewed
  exceptions to that rule: the first because its values must outlive every test
  in the binary, which a guard that restores on drop cannot express; the second
  because seeding a pre-existing value is the one thing the guard cannot do for
  itself. Both are bounded in `tests/env_lock_coverage.rs` by a write count
  *and* the region the writes must stay in, so neither is a general licence —
  do not move ordinary per-test setup into either.

## Project Structure

- Every test binary statically links the whole dependency graph, so related
  files are grouped into one binary each, using cargo's `tests/<dir>/main.rs`
  layout. Each file is a module of its group's `main.rs`, under its own name:
  - `tests/spend/` — `spend_enforcement`, `spend_headers`, `spend_metering`,
    `spend_metering_routing`, `spend_persistence`.
  - `tests/codex/` — `codex_spend_enforcement`, `codex_spend_metering`,
    `codex_spend_websocket`, `codex_websocket_fallback`, `codex_websocket_gated`.
  - `tests/inbound/` — `inbound_auth` and the `inbound_codex_*` files.
  - `tests/multi_account/` — `multi_account` and every `<provider>_multi_account`.
- Every other `tests/*.rs` file is a standalone binary. The other directories
  under `tests/` (`common/`, `judge_harness/`, `store_boot/`, `buffer_replay/`,
  `driven_lane/`, `response_coding_support/`) have no `main.rs`: they are
  support modules, not binaries.
- A new test file joins the matching group: put it in the group's directory and
  add `mod <name>;` to its `main.rs`. Make it a standalone `tests/*.rs` file
  only when it needs process-global state of its own (`antigravity_process.rs`
  sets env values that outlive every test; `count_tokens_warmup.rs` asserts a
  process-wide `OnceLock` is still cold), or when CI names it with `--test` (`router_surface`,
  `prefill_router`).
- A grouped binary declares a shared support module exactly once, in its
  `main.rs` (`#[path = "../common/mod.rs"] mod common;`, likewise
  `judge_harness`); its modules write `use crate::common;`. A second `mod
  common;` would compile a second env lock into the same process, and
  `tests/env_lock_coverage.rs` fails on it.
- Add protocol integration tests in `tests/` when behavior crosses module boundaries.
- Keep module-local unit tests inside the relevant `src/**` file for focused helper behavior.

## Code Style

- Make fixtures small but representative.
- Assert exact protocol fields for headers, status codes, event names, and tool-call IDs.
- Avoid live network calls; use Wiremock or pure fixtures.

## Git Workflow

- Include tests in the same commit as protocol behavior changes.
- Keep test names descriptive of the externally observable behavior.

## Boundaries

- ✅ Test regressions for every discovered protocol bug.
- ✅ Keep streaming and header-preservation tests strict.
- ✅ Bind the environment guard to a named local (`let mut vars = ...`), never
  to `_` — `let _ = ...` drops it before the test body runs.
- ⚠️ Ask before replacing integration coverage with mocks that cover less behavior.
- 🚫 Never delete or weaken tests to make a change pass.
