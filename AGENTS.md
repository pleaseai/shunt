# shunt Agent Instructions

## Build & Run Commands

- Build: `cargo build`
- Release build: `cargo build --release`
- Run: `cargo run -- run` or `./target/release/shunt run`
- Validate config: `cargo run -- check` or `./target/release/shunt check`
- Token helper: `cargo run -- token`

## Testing

- Full test suite: `cargo test --all-features --workspace`
- Format check: `cargo fmt --all --check`
- Lints: `cargo clippy --all-targets --all-features -- -D warnings`
- CI runs format, clippy, and tests with `RUSTFLAGS=-D warnings`.

## Project Structure

- `src/main.rs`: CLI entry point.
- `src/server.rs`: Axum router and endpoint registration.
- `src/proxy.rs`: request buffering, routing, adapter dispatch.
- `src/config.rs`: typed config, defaults, TOML/env loading, validation.
- `src/routing.rs`: exact route, prefix route, default-provider resolution.
- `src/adapters/`: provider protocol adapters.
- `src/model/`: Anthropic Messages and OpenAI Responses translation.
- `src/auth/`: credential lookup and refresh helpers.
- `tests/`: protocol and translation integration tests.
- `wiki/`: generated Astro Starlight documentation site.

## Code Style

- Write documentation and code in English.
- Keep Rust files focused and preferably under 500 lines.
- Preserve streaming semantics; do not buffer upstream SSE responses unless the client requested non-streaming output.
- Keep gateway-owned errors in Anthropic error shape.
- Prefer table-driven config additions over hardcoded provider logic.

## Git Workflow

- Use conventional commits.
- Keep PRs focused and link milestone docs when changing implementation behavior.
- Third-party GitHub Actions must be pinned to full commit SHAs.

## Boundaries

- ✅ Always preserve existing tests and add focused coverage for protocol changes.
- ✅ Always run format, clippy, and tests before reporting code changes as complete.
- ⚠️ Ask before changing credential-file writeback behavior.
- ⚠️ Ask before changing public config keys or documented provider semantics.
- 🚫 Never weaken or remove tests to make a change pass.
- 🚫 Never commit secrets, tokens, or generated local config files.
