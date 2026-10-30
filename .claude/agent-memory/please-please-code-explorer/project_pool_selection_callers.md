---
name: pool-selection-callers-map
description: Who calls AccountPool::select_order*, what session key each passes, where success/failover is known (snapshot 2026-10-31)
metadata:
  type: project
---

select_order_inner (src/accounts.rs ~950) is the single selection core; sticky slot = stable_session_index(sha256 mod distinct identities), blank id => round-robin counter `rr`. Needs-re-login mark is NOT read by selection (only snapshot + clear on mark_healthy*). Time is not injected: tests use real Instant/unix_now and backdate health fields directly.

Session key per caller: anthropic/mod.rs (claude_oauth, kimi) raw `x-claude-code-session-id` header; responses outbound (mod.rs, pool.rs) effective id (header | metadata session | hashed user-id), un-namespaced; inbound Codex endpoint (inbound.rs, inbound_routed.rs) `codex_endpoint::pool_sticky_key` = `{client}:{session}`; gemini passes None. codex_ws.rs never calls select_order (pool.rs picks account, keys socket `{account}\x1f{client}\x1f{session}\x1f{agent}`).

Success points: every adapter loop calls `mark_healthy*` at the Relay arm (gate on status.is_success()); failover = `continue`/cooldown arms. Verify before relying.
