# Memory index

- [shunt otel privacy-claim rot](shunt-otel-privacy-claim-rot.md) — `include_session_id` in `src/config.rs`/`src/telemetry.rs` was documented but never wired into `src/proxy.rs`; always grep a privacy/gating config field's name across the whole `src/` tree before trusting doc-comment claims about it.
- [responses adapter stream/JSON doc generalization](shunt-responses-adapter-stream-json-doc-generalization.md) — `src/adapters/responses/mod.rs` doc comments describe error handling from the SSE-streaming path's POV; `json_events_response` handles backend-sent `Ok`-wrapped error events asymmetrically (silently discards them), so "streamed through"/"error event" language can overclaim JSON-path parity.
