# tests Agent Instructions

## Build & Run Commands

- Run all tests from repository root: `cargo test --all-features --workspace`
- Run integration tests from repository root: `cargo test --test passthrough` and `cargo test --test responses_translate`
- Run quality gates from repository root: `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D warnings`

## Testing

- `passthrough.rs` uses Wiremock and a real Axum server to verify gateway HTTP behavior.
- `responses_translate.rs` verifies Anthropic Messages → OpenAI Responses request conversion and Responses SSE → Anthropic SSE conversion.
- Prefer behavior-focused assertions over implementation-only assertions.

## Project Structure

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
- ⚠️ Ask before replacing integration coverage with mocks that cover less behavior.
- 🚫 Never delete or weaken tests to make a change pass.
