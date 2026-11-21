---
name: project-pool-seams
description: Where the shunt Responses account pool forks per credential family, and why mid-stream rotation is feasible via a byte-level preamble peek
metadata:
  type: project
---

The Responses pool has three loops: `pool_events_stream` (streaming, src/adapters/responses/pool.rs ~399), `forward_chatgpt_oauth` (non-streaming, ~990) and inbound.rs (codex endpoint, ChatgptOauth only). Per-family forks are `if account.store_family == Some(Xai)` in `resolve_or_cooldown` and `classify_first`; `force_refresh_or_cooldown` is hard-wired to CodexAuthStore. `select_order_*` keeps cooling accounts in the order (deprioritized, not excluded), so "all accounts cooling" still ends in the last upstream 429 being mapped.

**Why:** the synthetic `message_start` is per-request (machine built before accounts), so rotating accounts before the first non-lifecycle upstream event is invisible to the client; early_stream.rs only commits the client response early and offers no rotate hook.

**How to apply:** new pooled families add arms in those forks and a sibling module (not more lines in pool.rs, which is already ~3800 lines); in-stream rotation = peek raw SSE bytes (SseParser::push) past `response.created`/`in_progress`, rebuild a `reqwest::Response` from the buffered prefix + remaining body.
