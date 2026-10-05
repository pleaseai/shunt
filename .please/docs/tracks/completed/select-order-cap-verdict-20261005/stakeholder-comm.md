## Acknowledgment

Hello. We have picked up issue #739. Here is how we understand it.

**Request**: When every account in a pool is held back by its usage cap, the gateway should report that consistently. Today it can occasionally send a generic "pool exhausted" error, with no retry time, instead of the cap error that says when to retry.
**Current status**: Scoped. The change keeps today's behavior and closes that rare mismatch.
**Impact**: Rare and narrow. Routing to the next upstream is unaffected. Only the final error a client sees, and its retry hint, can be wrong in a very short timing window.
**Next steps**: We will implement it and follow up when it is merged.

Let us know if anything here does not match what you expected. Thank you.

## Resolution

Hello. Issue #739 is fixed, and here is a summary.

**What was wrong**: The gateway decided which accounts it could use, then checked a second time whether usage caps were the reason none were available. If a cap cleared in the short gap between those two steps, the client got a generic "pool exhausted" error with no retry time, instead of the usage-cap error that says when to retry.
**How it was fixed**: The gateway now makes the cap decision in the same step that picks the accounts, so the error always matches that decision. The Antigravity pool also uses the same usage-cap error when caps block every account.
**What to expect**: Nothing changes in normal use. When every account in a pool is at its usage cap, the client gets the usage-cap error (HTTP 429 with `retry-after`), and requests still move on to the next configured upstream as before.
**Release status**: PR #743 is in review and will ship in a release after it merges.

Let us know if you see anything unexpected. Thank you.
