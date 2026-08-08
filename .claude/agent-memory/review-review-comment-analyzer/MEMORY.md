# Memory index

- [shunt otel privacy-claim rot](shunt-otel-privacy-claim-rot.md) — `include_session_id` in `src/config.rs`/`src/telemetry.rs` was documented but never wired into `src/proxy.rs`; always grep a privacy/gating config field's name across the whole `src/` tree before trusting doc-comment claims about it.
- [shunt "verbatim" convention](shunt-verbatim-terminology-convention.md) — shunt uses "verbatim" strictly for byte-identical passthrough; PR #114 had one loose use of it for a re-shaped error envelope.
- [responses adapter stream/JSON doc generalization](shunt-responses-adapter-stream-json-doc-generalization.md) — RESOLVED by PR #120 (issue #113): JSON paths now surface backend-sent error events as 502s via `AnthropicSseMachine::backend_error()`; recurring rot hotspot, recheck stream-vs-JSON parity language whenever these paths are touched again.
- [shunt account scan-cache comment rot](shunt-account-scan-cache-comment-rot.md) — Recheck lexical path collisions, discovery-only I/O claims, concurrent misses, and mtime invalidation language.
- [Codex WS continuation quantifier rot](codex-ws-continuation-quantifier-rot.md) — Reused continuation-enabled turns retrieve stored state; fresh, overflow, and full-retry turns bypass it, so avoid universal “every turn” claims.
- [shunt perf benchmark baseline scope](shunt-perf-benchmark-baseline-scope.md) — `perf_issues.rs` mixes frozen pre-#261 baselines with later issue-specific pairs; scope each baseline to its refactor.
- [shunt PR #331 auto-mode classifier clean](shunt-pr331-auto-mode-classifier-clean.md) — 3-round rewordings verified against `resolve_claude_account`/`forward_claude_oauth`; 7-surface cross-check method for gating-invariant claims.
