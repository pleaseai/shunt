---
name: pool-selection-callers-map
description: Who calls AccountPool::select_order*, what session key each passes, where success/failover is known (updated 2026-11-05)
metadata:
  type: project
---

select_order_inner (src/accounts.rs ~1254) is the single selection core. A session id is looked up in the pool's session binding table (src/accounts/session_binding.rs, keyed by provider + SHA-256 of the key); a bound, eligible account is the head. An unbound or moved session is placed by `assignment::place` (src/accounts/assignment.rs) using rendezvous ranking (`spread`) or priority then soonest weekly reset (`soonest_weekly_reset`); blank id => round-robin counter `rr`, no binding. Selection returns a ticket; `AccountPool::commit_session_binding` (src/accounts.rs ~1161) records the serving account on a 2xx. Needs-re-login mark is NOT read by selection (only snapshot + clear on mark_healthy*). Time is not injected: tests use real Instant/unix_now and backdate health fields directly.

Session key per caller: anthropic/mod.rs (claude_oauth, kimi) raw `x-claude-code-session-id` header; responses outbound (mod.rs, pool.rs) effective id (header | metadata session | hashed user-id), un-namespaced; inbound Codex endpoint (inbound.rs, inbound_routed.rs) `codex_endpoint::pool_sticky_key` = `{client}:{session}`; gemini passes None. codex_ws.rs never calls select_order (pool.rs picks account, keys socket `{account}\x1f{client}\x1f{session}\x1f{agent}`).

Success points: every adapter loop calls `mark_healthy*` and commits the binding (`SessionCommit`, src/adapters/session_commit.rs) at the Relay arm (gate on status.is_success()); failover = `continue`/cooldown arms. Verify before relying.
