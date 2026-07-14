---
name: pr122-retry-observability-gap
description: PR #122 (issue #48 bounded upstream retry, src/retry.rs) posture — ExceedsBudget give-up IS logged (verified); exhausted-retries give-up and one own_error() message-loss spot are NOT.
metadata:
  type: project
---

Reviewed PR #122 (`pleaseai/shunt`, issue #48 "bounded upstream retry/backoff") diff: `src/retry.rs` (new), `src/config.rs` (`RetryConfig`), `src/adapters/{anthropic,cursor,responses}/mod.rs`, `src/adapters/cursor/client.rs`, `src/metrics.rs`.

**Verified present (don't re-flag):** the `Backoff::ExceedsBudget` give-up branch in `send_with_retry` (retry.rs ~line 146) does emit `tracing::warn!("surfacing transient upstream response without retry: Retry-After exceeds max backoff")` — the task explicitly asked to confirm this and it is real, not swallowed.

**Real gaps found (reported as findings, confidence <70, so all `minor` tier):**

1. **Retries-exhausted give-up has no log at all** — the `other => return other` catch-all (retry.rs ~line 176) is reached both for `Ok(response)` with a still-retryable status when `retries_left` is false, and for a still-transient `Err` when `retries_left` is false. Neither path logs anything distinct from a normal outcome. For the `Ok(response)` case specifically, the request then flows through `proxy.rs`'s `Ok` branch and is logged at `tracing::info!("proxied request", upstream_status=…)` — indistinguishable from a normal success except for the status field. This is asymmetric with the ExceedsBudget branch, which the PR's own comments justify logging specifically for operator visibility into the give-up path — the same reasoning applies to plain exhaustion but wasn't applied there.

2. **`forward_http`'s post-retry error path (responses/mod.rs ~line 693) still funnels through `own_error(error.to_string())`**, whose `AdapterError.message` is hardcoded to the generic `"responses adapter failed"` (see `own_error` ~line 1322) — so `proxy.rs`'s top-level `tracing::warn!("upstream request failed", error = %error.message)` catch-all loses the real reqwest error text for the single-credential Responses path after retries are exhausted. Notable because this same PR *did* fix the identical generic-message problem at two nearby `forward_chatgpt_oauth` log sites (changed `error = %error.message` → `error = %error`, responses/mod.rs ~lines 280/443) but left it in `forward_http`, which is actually the more central, more-often-hit path this PR targets. Same root cause as the pre-existing [[shunt-codex-ws-error-handling]] PR #39 finding #2 (own_error() message-hardcoding), recurring at a new call site.

3. **Low-confidence/minor**: `provider_retry_policy` (responses/mod.rs) and the Cursor policy/base_url lookups (cursor/mod.rs ~lines 110-119) silently fall back to `RetryPolicy::DISABLED` / a default base_url if `route.provider` isn't found in config, whereas the sibling Anthropic-adapter code enforces the identical "provider was validated at routing" invariant with a loud `.expect(...)` panic. Low severity since this mirrors a pre-existing convention already used for `auth`/`base_url` resolution in the same files.

**How to apply:** if re-reviewing a follow-up to src/retry.rs or these adapter files, check whether an explicit "retries exhausted, giving up" log was added to `send_with_retry`, and whether `forward_http`'s error mapping was changed to preserve the real error text. Related: [[shunt-codex-ws-error-handling]] (same own_error pattern, different call site).
