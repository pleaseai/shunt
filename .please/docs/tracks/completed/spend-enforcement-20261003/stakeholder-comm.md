## Resolution report

Hello, the spend limits requested in #728 are now enforced. This is the status.

**What was missing**: The per-user and organization spend caps were already stored, but nothing applied them. Every caller could spend through the gateway's shared upstream credentials without a ceiling.

**What changed**: Requests to `POST /v1/messages` now check the caller's daily, weekly and monthly caps before the gateway sends anything upstream. Every paid upstream call is priced and counted against the caller, and the counts survive a restart. A caller who has reached a cap gets a clear "spend limit reached" error that names the reset time. Capped callers also see their own limit in the `anthropic-ratelimit-unified-*` response headers. A new endpoint, `GET /v1/organizations/spend_limits/effective`, shows each user's cap and spend so far.

**How to check it**: Set a small daily `user` cap for a token and send requests until the cap is used up. The next request returns `429` with the reset time. The effective-limits endpoint shows the spend you have used.

**Known gaps**: The Codex endpoint (`/v1/responses` and WebSocket) is not enforced yet (#733). Counters stop growing at about $18,400 per user per window (#736).

**Deployment status**: PR #732 is in review. The change ships in the next release after it merges.

If you see anything unexpected, please let us know. Thank you.
