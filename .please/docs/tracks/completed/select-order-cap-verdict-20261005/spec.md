# Report cap exhaustion from account selection

> Track: select-order-cap-verdict-20261005
> Issue: #739

## Overview

When every selectable account in a pool is held back by its `max_utilization*` hard cap, the gateway answers with a cap-specific `429` that carries `retry-after` and advances `[[upstreams]]` failover. Today the selection step decides which accounts are capped, then throws that verdict away. Each pool adapter's exit then asks the pool a second time, under a second lock, whether the empty result was caused by caps.

That second pass has three costs:

- **The verdict can disagree with the selection it explains.** A cap can clear between the two passes, through a window reset or a lower concurrent observation. The client then gets the generic exhaustion error, with no `retry-after`, instead of the cap `429`.
- **The selectability rules exist twice and must be kept in sync by hand.** These rules are disabled, paused, and one representative per alias identity. PR #738 needed two review fixes for exactly this drift.
- **Every adapter has to remember to ask.** An adapter that forgets silently returns the generic error. The Gemini/Antigravity adapter never asks today.

This track makes the selection step return its own cap verdict alongside the order, from the same pass that built the order. Every pool adapter reads that verdict on its empty-order exit, including Gemini/Antigravity.

## Scope

- Selection produces the cap-exhaustion verdict while building the order, and returns it with the order. The verdict holds the earliest known eligibility time among the selectable representatives (not disabled, not paused) that caps excluded.
- Every pool adapter's empty-order exit uses that returned verdict. This covers:
  - Claude OAuth
  - Kimi OAuth
  - the Codex/ChatGPT HTTP loop
  - the Codex/ChatGPT pooled event stream
  - the inbound Codex passthrough
  - Gemini/Antigravity
- The separate after-the-fact cap check is removed from every production path. Where existing unit tests call it directly, it stays as a test-only helper that derives its answer from the selection verdict. It does not re-implement the rules.
- The Known Issue "Cap re-check after selection" in the per-window `max_utilization` track plan is marked resolved. Tech-debt entry TD-001 is closed.

## Success Criteria

- [ ] SC-1: When caps empty a pool, the cap `429` and its `retry-after` come from the same selection pass that produced the empty order. No second pass over the pool remains on any production exit path.
- [ ] SC-2: Externally observable behavior is unchanged, apart from closing the race described in the Overview, where a cap that clears between the two passes used to produce the generic error:
  - the HTTP status
  - the error type and message
  - `retry-after`
  - the failover advance
  - the rotation metric reason (`capped` vs `exhausted`)
  - the committed-stream terminal `error` event
  - the inbound Codex OpenAI error shape
- [ ] SC-3: Every existing cap test passes without modification. That covers the cap tests in the multi-account, failover, Codex multi-account, inbound Codex endpoint, Kimi multi-account, and Codex WebSocket fallback suites, and the account-pool unit tests.
- [ ] SC-4: Each selectability rule (disabled, paused, alias representative) is defined in one place. A regression test shows that, when caps empty the pool, the cap verdict agrees with the selection's own exclusions even if the quota state changes after selection returns.
- [ ] SC-5: The Gemini/Antigravity pool returns the cap `429` when caps empty it. This is shown with injected utilization, because no Antigravity utilization source exists today.

## Constraints

- No change to public config keys, error wire shapes, documented provider semantics, or credential-file writeback.
- Streaming semantics are preserved. No upstream response is buffered.
- Existing tests are neither weakened nor rewritten. New tests may be added.
- The hot selection path gains no extra lock acquisition and no extra pass over the accounts.

## Out of Scope

- Changing what counts as capped, how `eligible_at` is computed, or the `retry-after` floor.
- Feeding Antigravity quota into caps. This track wires only the exit.
- Per-alias cap evaluation on `/usage` and the admin snapshot (TD-002).
- The `/api/oauth/usage` fallback's handling of paused accounts.

## Assumptions

- Keeping a test-only helper with the old cap-check name satisfies "existing unit tests pass unchanged" without keeping a second production rule set.
- The Gemini/Antigravity exit can reuse the same gateway cap error as the other adapters, in the Anthropic error shape. It needs no provider-specific variant.
