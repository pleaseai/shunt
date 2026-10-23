## Acknowledgment

Thanks for the detailed report in #817. Here is how we understand it.

**What you reported**: When an Anthropic API key runs out of money (its spend limit or its credit balance), shunt returns Anthropic's refusal to the client instead of moving on to the next upstream in your chain, so your second API key and your Claude subscription pool are never tried.

**Current status**: Confirmed. Today shunt treats every HTTP 400 as a request error that no other upstream could fix, and Anthropic sends these refusals as 400 (or 402).

**Impact**: A chain ordered "API key first, then subscription" stops at the first exhausted key. The workaround you found, removing the API upstreams, works but gives up the API credit.

**Next step**: We are changing shunt to recognise three account-level refusals: a spend limit you set, an exhausted credit balance, and a billing error. On these, shunt moves to the next upstream and skips the refusing key until access resumes, or for 1 hour when Anthropic gives no time. Every other 400 still stops the chain. No timeline promised.

One thing to expect: the monthly API credit that comes with a Max plan does not cover Claude Code. When Claude Code is the client, expect that credit to be refused; with this change, the request then moves on to your subscription pool.

## Resolution

Thanks again for the report in #817. The fix is ready for review.

**What was wrong**: shunt treated every HTTP 400 from Anthropic as a problem with the request, one that no other upstream could fix. Anthropic reports an exhausted spend limit or credit balance as a 400 (or 402), so shunt passed that refusal straight back to the client and never tried the next upstream in your chain.

**What we changed**: shunt now recognises three account-level refusals from Anthropic: a spend limit you set (organisation or workspace), a credit balance that is too low, and a billing error. When one of these comes back, shunt moves on to the next upstream. It also skips the refusing key for new requests until the time Anthropic says access resumes, or for 1 hour when Anthropic gives no time. In a Claude subscription pool, the refusing account is set aside for the same window and the next account takes the request. Every other 400 still stops the chain, so a malformed request is not resent to every upstream.

**What to expect**:
- With an "API key first, then subscription" chain, a request that hits an exhausted key is now served by the next upstream, with no client-side change.
- The gateway log shows one warning per refusal, `upstream refused the account; skipping it until the window ends`, with the upstream name, the kind of refusal, and when the skip ends.
- The skip is kept in memory, so restarting shunt clears it.
- If every upstream refuses, you still get Anthropic's own refusal message.
- As noted earlier, the monthly API credit that comes with a Max plan does not cover Claude Code, so Claude Code traffic on that credit is refused and moves on to your subscription pool.

**Release status**: The fix is in PR #825, which is under review. It ships in the first release after that PR is merged.

Sorry for the trouble this caused. If anything still doesn't behave as described, please let us know.
