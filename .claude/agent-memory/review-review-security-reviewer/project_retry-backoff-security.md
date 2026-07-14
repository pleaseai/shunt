---
name: retry-backoff-security
description: src/retry.rs bounded upstream retry (issue #48) security posture — all amplification/leak/sleep vectors mitigated; verified-safe list for future retry-related reviews.
metadata:
  type: project
---

`src/retry.rs` `send_with_retry` (issue #48, PR feat/retry) — bounded, pre-stream, idempotent upstream retry on transient failures (429/502/503/504 + connect/timeout). Wired into anthropic/mod.rs, responses/mod.rs (forward_http, single-cred only), cursor/mod.rs. OAuth pools (claude_oauth/chatgpt_oauth) deliberately excluded — they have account-rotation failover.

**Verified-safe (don't re-flag these on retry changes):**
- **Retry-storm bound**: `max_retries <= 10` enforced in `Config::validate()` (config.rs `MAX_RETRIES_LIMIT`), called at startup (main.rs) + hot-reload (reload.rs). `multiplier >= 1.0 && finite` also validated.
- **Attacker Retry-After long-sleep**: `next_backoff` caps honored `Retry-After` at `max_backoff` → `Backoff::ExceedsBudget` → surfaces response immediately, never sleeps past budget. `retry_after()` (accounts.rs, pre-existing) parses integer seconds only; `Duration::from_secs(u64)` can't overflow to a real sleep because the > max_backoff check fires first.
- **Token/credential logging**: retry `warn!` logs carry only provider/status/attempt/max_retries/delay_ms (status branch) or `error = %error` (transport branch). `reqwest::Error` Display exposes URL only (base_url, no userinfo, no secret) — never headers/body, so the injected bearer/api-key never lands in logs. Retry metric `shunt.upstream_retries` tags = provider + low-card reason (429/502/503/504/transport) only.
- **Wrong-host credential leak / SSRF**: `url` computed once before the loop from provider-config base_url; headers+body cloned to the SAME url each attempt. No user-controlled URL introduced.
- **Client-driven amplification**: retries fire only on upstream transient responses the inbound client can't directly force; factor bounded to `max_retries+1` (default 3, max 11).

**Why:** task #48 review asked to confirm these five vectors; all mitigated. **How to apply:** if a future change loosens the max_retries cap, removes the ExceedsBudget branch, adds body/headers to retry logs, or makes base_url per-attempt, re-open the corresponding vector. Related: [[claude-token-url-egress]] (base_url guard), [[sentry-pii-egress]] (log egress).
