## Acknowledgment

Hello. We have picked up issue #739. Here is how we understand it.

**Request**: When every account in a pool is held back by its usage cap, the gateway should report that consistently. Today it can occasionally send a generic "pool exhausted" error, with no retry time, instead of the cap error that says when to retry.
**Current status**: Scoped. The change keeps today's behavior and closes that rare mismatch.
**Impact**: Rare and narrow. Routing to the next upstream is unaffected. Only the final error a client sees, and its retry hint, can be wrong in a very short timing window.
**Next steps**: We will implement it and follow up when it is merged.

Let us know if anything here does not match what you expected. Thank you.
