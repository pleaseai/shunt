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
