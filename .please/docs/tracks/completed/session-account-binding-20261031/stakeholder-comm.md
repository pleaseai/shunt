## Resolution Report

Hello. The issue you reported has been fixed, and here is what changed.

**What was wrong**: With several accounts pooled for one provider, a conversation's account was derived from its session id. Whenever that account could not serve, every affected conversation fell back to the same next account and then moved again together. Conversations switched accounts far more often than needed, often while their prompt cache was still warm, so they lost that cache.

**How it was fixed**: A conversation now stays on the account that last served it successfully. It moves only when that account cannot serve, or after a full minute of running close to its quota limit. Assignments survive a gateway restart when `state_path` or `[store]` is configured; without either, they remain in memory. A new `[server.pool] assignment` setting chooses how a new conversation is first placed.

**What to expect / how to check**: Within a pool, a conversation should usually keep one account, but soft-near reassignment, failover, 24-hour idle expiry, or LRU eviction can place it elsewhere. `GET /admin/api/pool` shows how many conversations each account holds (`bound_sessions`), and, with OpenTelemetry export on, the `shunt.pool.session_account_switches` metric counts every move and its reason.

**Release status**: PR #884 is open for review. The fix ships in the next release after it merges.

Thank you for the detailed measurements. If you still see unexpected account switches after upgrading, please let us know.
