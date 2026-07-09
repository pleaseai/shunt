# src Agent Instructions

## Build & Run Commands

- Build from repository root: `cargo build`
- Run from repository root: `cargo run -- run`
- Validate config from repository root: `cargo run -- check`

## Testing

- Full suite from repository root: `cargo test --all-features --workspace`
- Source-focused checks: `cargo clippy --all-targets --all-features -- -D warnings`
- Format check: `cargo fmt --all --check`

## Project Structure

- `main.rs`: CLI entry point and process lifecycle.
- `server.rs`: Axum router and shared state.
- `proxy.rs`: request buffering, route resolution, adapter dispatch.
- `config.rs`: configuration schema, defaults, loading, validation.
- `routing.rs`: model-to-provider route selection.
- `adapters/`: provider protocol boundaries.
- `model/`: Anthropic Messages ↔ OpenAI Responses translation.
- `auth/`: credential lookup and token refresh.

## Code Style

- Keep files focused and preferably under 500 lines.
- Preserve streaming behavior for upstream responses.
- Keep protocol-specific behavior inside adapters and model translation modules.
- Use typed config and explicit enums for externally visible modes.

## Git Workflow

- Pair code behavior changes with tests.
- Keep implementation PRs aligned with docs under `docs/` when behavior changes.
- Use conventional commits.

## Boundaries

- ✅ Add focused unit tests near changed modules.
- ✅ Preserve Anthropic error envelope for gateway-owned errors.
- ⚠️ Ask before changing credential refresh/writeback semantics.
- ⚠️ Ask before changing public config keys.
- 🚫 Never remove streaming tests or weaken protocol assertions.
