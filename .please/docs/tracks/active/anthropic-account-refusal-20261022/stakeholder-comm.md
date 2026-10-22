## Acknowledgment

Thanks for the detailed report in #817. Here is how we understand it.

**What you reported**: When an Anthropic API key runs out of money (its spend limit or its credit balance), shunt returns Anthropic's refusal to the client instead of moving on to the next upstream in your chain, so your second API key and your Claude subscription pool are never tried.

**Current status**: Confirmed. Today shunt treats every HTTP 400 as a request error that no other upstream could fix, and Anthropic sends these refusals as 400 (or 402).

**Impact**: A chain ordered "API key first, then subscription" stops at the first exhausted key. The workaround you found, removing the API upstreams, works but gives up the API credit.

**Next step**: We are changing shunt to recognise three account-level refusals: a spend limit you set, an exhausted credit balance, and a billing error. On these, shunt moves to the next upstream and skips the refusing key until access resumes, or for 1 hour when Anthropic gives no time. Every other 400 still stops the chain. No timeline promised.

One thing to expect: the monthly API credit that comes with a Max plan does not cover Claude Code. When Claude Code is the client, expect that credit to be refused; with this change, the request then moves on to your subscription pool.
