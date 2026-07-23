# Auto-linked dashboard observation

## Problem

The admin dashboard currently requires users to provision separate managed pool accounts merely to see usage, and labels rows by store filenames rather than recognizable identity. This is disconnected from credentials the running shunt already uses.

## Tickets

### OBS-1 — Read-only local credential observation

- **Do:** Discover Claude Code credentials (configured file, default file, and macOS Keychain fallback) and Codex CLI `auth.json`. Parse only display metadata and a current access token. Never refresh or write source credentials.
- **Files:** `src/auth/observation.rs`, `src/auth/mod.rs`
- **Done when:** focused tests prove parsing, expiry classification, identity masking, and no refresh-token field enters the returned model.
- **Depends on:** none
- **Wave:** 1

### OBS-2 — Admin observed-usage API

- **Do:** Add authenticated `GET /admin/observed`; fetch Claude usage with the current access token only, report Codex as response-derived, and return explicit unavailable reasons. Tokens never leave the process.
- **Files:** `src/admin/mod.rs`, tests
- **Done when:** authenticated endpoint tests cover absent, expired, and available sources; unauthorized requests remain rejected.
- **Depends on:** OBS-1
- **Wave:** 2

### OBS-3 — Usage-first dashboard

- **Do:** Make “Accounts and usage” the primary view, show observed identity/source/signal/state, and collapse managed account provisioning under an advanced disclosure.
- **Files:** `src/admin/html.rs`, docs
- **Done when:** HTML tests assert usage-first ordering, collapsed management, recognizable labels, and no “Pool health” title.
- **Depends on:** OBS-2
- **Wave:** 3

## Invariants

- Observation never refreshes, writes, copies, or returns a source credential.
- Expired credentials are displayed as unavailable with a re-login hint.
- Store-managed pool accounts remain a separate ownership lane.
- Provider signal differences are explicit; missing data is never rendered as 0%.
