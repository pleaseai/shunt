# Status

Last updated: 2026-07-23

## Stage

Provider-native observation implemented and verified for six provider families; not committed or pushed.

## Recent decisions

- Usage observation and managed pool provisioning are separate lanes.
- Observation is automatic and strictly read-only: never refresh or write discovered credentials.
- Claude Code discovery supports configured/default credential files and macOS Keychain.
- Codex CLI discovery uses the same `CODEX_AUTH_FILE`/`~/.codex/auth.json` source as inference.
- Claude usage snapshots are cached for 60 seconds and keyed by SHA-256 of the access token so account switches cannot reuse another account's snapshot.
- Unpooled Codex traffic captures `x-codex-*` quota under the access token's stable account id on both translated Messages and raw Responses paths.
- Managed account forms, store metadata, and filename-centric pool health are collapsed under **Manage pool accounts (advanced)**.
- Gemini renders every Code Assist quota bucket returned by `retrieveUserQuota`; Kimi renders weekly and 5-hour limits.
- Grok reads first-party billing/product usage and account tier from the Grok CLI OAuth surface.
- Cursor opens Cursor.app's SQLite state read-only, derives its first-party session in memory, and renders billing-cycle, Auto + Composer, and named-model usage.
- The dashboard mirrors the personal site's Fragment Mono, blue radial background, glass cards, and light/dark palette; provider marks are inline local SVGs and quota bars retain exact text plus accessible progress semantics.

## Verification

- Focused observation tests — passed (12 tests, including Gemini, Kimi, Grok, and Cursor wiremock contracts).
- `cargo test --all-features --workspace` — passed (820 library tests plus every integration and doc-test target).
- `cargo clippy --all-targets --all-features -- -D warnings` — passed.
- `cargo fmt --all --check` and `git diff --check` — passed.
- Live preview on `127.0.0.1:3021` verified real Claude, Grok, Kimi, and Cursor usage; Codex waiting-for-traffic and expired Gemini states render honestly. The real gateway on `127.0.0.1:3001` remained untouched.
- Desktop and 390px mobile Playwright renders verified six provider logos, eight accessible progress bars, no horizontal page overflow, and stacked mobile rows; screenshots are `/tmp/shunt-dashboard-redesign.png` and `/tmp/shunt-dashboard-redesign-mobile.png`.
- Independent reviewer dispatch failed before producing a verdict because the external model was rate-limited.

## Outstanding TODOs

- Obtain an independent reviewer pass when external-model quota is available.
- Decide whether to commit this slice with the existing P0 setup commit or as a separate commit.
- Integrate/rebase with sibling `feat/usage-oauth-endpoint` before promising native Claude Code `/usage` synthesis.

## Next action

Show the exact commit plan and commit only with approval. Do not push unless explicitly requested.
