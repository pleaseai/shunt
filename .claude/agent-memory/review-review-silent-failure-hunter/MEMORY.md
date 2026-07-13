# Memory index

- [shunt Codex WS error handling](shunt-codex-ws-error-handling.md) — PR #39 (Codex WebSocket v2 transport): silent-truncation-on-close bug + generic-only fallback log bug found in src/adapters/codex_ws.rs and responses.rs.
- [PR #85 admin surface error handling](pr85-admin-surface-error-handling.md) — src/admin/mod.rs systematically discards real errors (store I/O, OAuth exchange, token persist) with zero tracing, breaking codebase convention; worst at complete_account after single-use code exchange.
