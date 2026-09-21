# ChatGPT/Codex backend prompt-cache affinity

The ChatGPT backend derives prompt-cache affinity from the Responses `session-id` request header (`codex-rs` `core/src/client.rs`, `responses_session_id`: "ChatGPT derives cache affinity from the Responses session-id header"). The body `prompt_cache_key` must equal that header's value; the real Codex CLI sends its raw session id in both.

shunt's outbound request (chatgpt-oauth branch of `src/adapters/responses/request.rs`, WS handshake in `websocket.rs`) carries:

- `session-id` + `thread-id` headers = the effective conversation id, in order: the inbound `x-claude-code-session-id` header, then `metadata.user_id` JSON `session_id`. A metadata-only client (no session header) gets the headers derived from metadata.
- body `prompt_cache_key` derived from the same effective id (`effective_session_id` in `src/model/responses_request.rs`). A plain `user_id`, a missing JSON `session_id`, or a session string that cannot be a header value (an escaped control character) falls back to a stable Sha256-8-byte hash of the raw `user_id` — hex, header-safe, and the same value on both sides, so header and body key are always equal.
- `x-client-request-id` + `x-codex-window-id` (kept from the earlier codex-header set).

Measured 2026-09-20 against the ChatGPT backend (gpt-5.6-luna/sol/terra via chatgpt, gpt-6-astra via codex): before the `session-id` header landed, the backend reported zero `cached_tokens` and zero `cache_write_tokens` on every turn — 20+ probes across streaming/non-streaming, 0.3K–11K prompts, effort/thinking/tools variants, byte-identical repeats. shunt-side forwarding and usage mapping were proven correct against a local mock upstream, so the header absence was the affinity break.

Measured after the fix (gpt-5.6-luna, ~59K-token stable system prompt): the cold turn reports zero cache, the exact replay reads 58,112/59,236 (98.1%) and an appended turn keeps the same read — matching `openai/codex#44716`, where fresh sessions report zero on the first request and reuse on exact replays. The metadata-only client shape reads the same rate. The WebSocket transport was also tested and changed nothing. Small prompts (~300–1500 tokens) still report zero cache activity — the backend's minimum-cacheable size sits somewhere between those and ~35K tokens.

The ChatGPT backend does not report `cache_write_tokens` at all (`cache_creation_input_tokens` stays 0; only `cache_read` is populated), so hit-rate aggregates must divide `cache_read` by input tokens, not by read+creation.

Known deliberate deviations from the Codex CLI request: the api-key provider branch sends no session headers; `include` stays gated on extended thinking; `tool_choice` preserves the client's choice; `client_metadata`, `service_tier`, `stream_options` and codex's always-`auto` tool choice are not replicated. `text.verbosity` is always `medium`.
