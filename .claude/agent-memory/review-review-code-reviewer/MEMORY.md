# Memory index

- [shunt Codex WebSocket v2 (PR #39)](shunt-codex-websocket-v2.md) — architecture; PR #39's silent-phantom-success bug RESOLVED as of PR #111 (verified 2026-07-14).
- [shunt Codex WS peek/fallback (issue #46, PR #111)](shunt-codex-ws-peek-fallback-issue46.md) — always-peek-first-event design, the one real (minor) latency tradeoff, and a pre-existing json_events_response discard-bug not introduced by this PR.
- [shunt responses adapter](project_shunt_responses_adapter.md) — AGENTS.md rules for src/adapters/responses.rs and how to verify "real Codex CLI" header claims via GitHub code search.
- [shunt Anthropic multi-account refresh_lock (PR #70)](shunt-anthropic-multi-account-refresh-lock.md) — over-broad refresh_lock scope in forward_claude_oauth() + untested RefreshRetry non-401 retry fallthrough.
- [shunt Sentry transaction bypass](project_shunt_sentry_transaction_bypass.md) — sentry-rust 0.48.4 has no before_send_transaction; hostname-leak fix verified against vendored crate source.
- [codex-subagent msgstart-input review](shunt-codex-subagent-msgstart-input-review.md) — PR #112: branch-diff gotcha; zero-sentinel bug and unconditional-compute/double-parse nit both FIXED as of d1eda88.
- [issue #48 retry/backoff (PR #122)](shunt-issue48-retry-backoff-pr122.md) — retry.rs pre-stream/OAuth-exclusion invariants, per-adapter count_tokens exclusion shape, and CONFLICTING-mergeable-does-not-mean-broken-code gotcha.
