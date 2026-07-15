# Memory index

- [shunt otel privacy-claim rot](shunt-otel-privacy-claim-rot.md) — `include_session_id` in `src/config.rs`/`src/telemetry.rs` was documented but never wired into `src/proxy.rs`; always grep a privacy/gating config field's name across the whole `src/` tree before trusting doc-comment claims about it.
- [shunt "verbatim" convention](shunt-verbatim-terminology-convention.md) — shunt uses "verbatim" strictly for byte-identical passthrough; PR #114 had one loose use of it for a re-shaped error envelope.
- [responses adapter stream/JSON doc generalization](shunt-responses-adapter-stream-json-doc-generalization.md) — RESOLVED by PR #120 (issue #113): JSON paths now surface backend-sent error events as 502s via `AnthropicSseMachine::backend_error()`; recurring rot hotspot, recheck stream-vs-JSON parity language whenever these paths are touched again.
- [shunt account scan-cache comment rot](shunt-account-scan-cache-comment-rot.md) — Recheck lexical path collisions, discovery-only I/O claims, concurrent misses, and mtime invalidation language.
