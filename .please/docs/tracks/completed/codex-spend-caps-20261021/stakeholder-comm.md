## Acknowledgment

Hello. We have received your request and logged it. Here is our understanding of it.

**Request**: Spend caps currently apply only to Claude-style requests. Requests through the Codex endpoint skip both the cap check and cost tracking, so a user who has reached a cap can keep spending there. You asked for that endpoint to enforce the same caps and count its cost toward them.

**Current status**: Confirmed. The gap follows from how the previous spend-limit change was scoped, and the requirements are now written down.

**Impact**: Operators who turn on spend caps and also run the Codex endpoint cannot rely on those caps, because spend through Codex goes uncounted and unblocked. Deployments that do not use spend caps are not affected.

**Next steps**: We will plan the change so that:
- every Codex request, including WebSocket sessions and web search, is checked against the user's caps;
- refused requests return an error the Codex CLI shows as a usage limit, with the time the limit resets;
- completed requests add their cost to the same counters, so one cap covers both endpoints.

We will reply once a pull request is ready for review.

One note: shunt has no built-in prices for GPT/Codex models. Until operators add pricing overrides for those models, their cost will be estimated at the default rate for unknown models.

If any of this does not match what you need, please let us know. Thank you.

## Resolution

Hello. The change you requested is ready for review. Here is a summary.

**What was wrong**: Spend caps applied only to Claude-style requests. Requests through the Codex endpoint skipped both the cap check and cost tracking, so a user who had reached a cap could keep spending there.

**What we changed**: The Codex endpoint now checks every request against the same per-user and per-organization caps, and adds each request's cost to the same daily, weekly and monthly counters. This covers regular requests, WebSocket sessions and web search. A request over a cap gets an error the Codex CLI shows as a usage limit, with the time the limit resets. A request that ends early, before it reports usage, is charged an estimate of its input plus the output it already returned.

**How to check it**: With spend caps turned on and the Codex endpoint enabled:
- set a small cap for a user and send a few Codex requests;
- the user's spend under the spend-limit admin view goes up with each completed request;
- once the cap is passed, the Codex CLI reports a usage limit and the request is not sent to the upstream;
- on a WebSocket session, only the refused turn ends; the session stays open.

**Deployment status**: Pull request #814 is in review. The change is not deployed yet; it will ship in a release after the pull request merges.

One note from earlier still applies: shunt has no built-in prices for GPT/Codex models. Until you add pricing overrides for those models, their cost is estimated at the default rate for unknown models.

If you find anything that does not work as described, please let us know. Thank you.
