---
name: codex-ws-continuation-quantifier-rot
description: Codex WebSocket continuation comments can overstate retrieval as universal even though fresh, overflow, and full-retry turns bypass the stored-state clone.
metadata:
  type: project
---
Codex WebSocket continuation documentation is prone to universal-quantifier rot: stored continuation retrieval applies to reused, continuation-enabled turns, not every pooled or every Codex turn; fresh and overflow turns return before reading the slot, and the full-input retry disables continuation lookup.

**Why:** PR #270 introduced performance-rationale comments using “every turn” and “the first thing every pooled Codex turn does,” while `start_ws_turn` and `Turn::stored_continuation` retain explicit bypass paths.

**How to apply:** When reviewing continuation performance comments, trace the fresh/reused/overflow and `allow_continuation = false` paths before accepting “every,” “always,” or “first” claims.
