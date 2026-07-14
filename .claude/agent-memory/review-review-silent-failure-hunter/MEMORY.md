# Memory index

- [shunt Codex WS error handling](shunt-codex-ws-error-handling.md) — PR #39 (Codex WebSocket v2 transport): silent-truncation-on-close bug + generic-only fallback log bug found in src/adapters/codex_ws.rs and responses.rs.
- [PR #85 admin surface error handling](pr85-admin-surface-error-handling.md) — src/admin/mod.rs systematically discards real errors (store I/O, OAuth exchange, token persist) with zero tracing, breaking codebase convention; worst at complete_account after single-use code exchange.
- [PR #112 message_start estimate](pr112-message-start-estimate.md) — responses.rs `finish()`/`usage_value()` never falls back to `input_tokens_estimate`, so a truncated stream's final usage silently reverts to 0 after message_start showed a nonzero estimate; same root cause as PR #39's finish()-ignores-truncation bug.
- [Issue #113 status threading](issue113-responses-status-threading.md) — backend-error-event fix (89e022b) + iteration-2 status.status() threading at 4 non-streaming call sites in responses/mod.rs, both verified correct 2026-07-14.
