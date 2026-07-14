---
name: shunt-pool-burn-rate-coverage
description: PR #136 (issue #135) src/accounts.rs+config.rs burn-rate load balancer gap analysis — headline "order healthy accounts by headroom" path untested, plus all-disabled and multi-window aggregation gaps.
metadata:
  type: project
---

PR #136 (`amondnet/135`, commit d76b7c2) adds per-account/per-window soft
quota thresholds + burn-rate-aware ordering + priority/disabled to
`src/accounts.rs` (+649) and `src/config.rs` (+236). Unusually thorough
`#[cfg(test)]` coverage already exists (9 new unit tests in accounts.rs, 3 in
config.rs) — most of the obvious matrix (threshold resolution incl. hard-cap,
window_headroom pure-function cases, account-threshold-override rotation,
burn_rate_avoidance on/off, priority in both pool modes, all-near guard,
hard-vs-soft ordering, disabled exclusion, snapshot field reporting) is
covered. Real gaps found:

1. **Headline feature untested for its actual case**: `select_order`'s
   `available_under.sort_by` (Some(pool) branch) orders *healthy* (under
   soft-threshold) equal-priority accounts by burn-rate headroom
   (`src/accounts.rs` ~line 621-631) — this is literally what "burn-rate
   aware ordering" (not just avoidance) means per the PR title/docs. Every
   existing headroom test (`all_near_accounts_fall_back_to_headroom_order`,
   `burn_rate_avoidance_rotates_fast_burning_sticky_account`) exercises
   headroom ordering only among *near*-quota accounts (`near_soft` bucket),
   never among two still-under-threshold available accounts with equal
   priority but different headroom. Distinguish "near ordering tested" from
   "available ordering untested" on any future burn-rate/headroom PR here.

2. **All-accounts-disabled (non-empty list) is unhandled by any test.**
   `select_order` filters disabled accounts out of `rotation`; if every
   configured account is disabled, `rotation` is empty and the function
   returns an empty `Vec` (distinct from the explicit `accounts.is_empty()`
   early-return). The caller (`forward_claude_oauth` in
   `src/adapters/anthropic/mod.rs`) then never enters its `for index in
   order` loop, `last_response` stays `None`, and it falls through to the
   generic `"all Claude OAuth accounts failed before receiving an upstream
   response"` error — misleading (nothing was attempted) but not a crash.
   No config-time guard rejects "every account disabled" either.

3. Minor: multi-window `min()` headroom aggregation in `assess_quota` (5h
   AND weekly both contributing) untested in combination; NaN threshold
   rejection is explicitly named in a code comment as the reason for the
   boot-time range check but no test feeds NaN through TOML; only one of
   the four per-window keys (`threshold_5h`) is exercised by
   `validate_rejects_out_of_range_account_thresholds` (mechanical, low risk).

See also [[shunt-inbound-auth-multislot-coverage]] and
[[shunt-codex-multi-account-coverage]] for the recurring "wiring/новых code
path looks tested because a sibling path is tested" trap — same shape here
(near-bucket headroom tested ≠ available-bucket headroom tested).
