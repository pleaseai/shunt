---
date: 2026-10-04
topic: per-window-max-utilization
---

# Per-window hard utilization caps for pool accounts

## Summary

Add per-account, per-window hard utilization caps (`max_utilization_*`) for the 5h, 7d, and Fable (`7d_oi`) quota windows. A request skips any account whose governing window is at or past its cap. When the whole pool is capped, the request fails over to the next upstream or returns a gateway-owned rate-limit error.

---

## Problem Frame

A team routes Claude Code through a shunt Claude OAuth pool and relies mainly on Opus. Fable traffic has been draining the pool's accounts faster than intended. Operators have no way to say "Fable may use this account up to X, and no further."

The existing per-window knobs (`threshold_*`, `default_threshold_*`, `hard_threshold`) are soft. They reorder accounts, and an account past every threshold is still selected once its peers are also past theirs. The only ways to exclude an account today are `disabled = true` and a runtime pause. Both are manual and all-or-nothing across models, and the pause is lost on restart.

---

## Key Decisions

- **A new knob, not a change to existing thresholds.** Soft thresholds and `hard_threshold` keep their current reorder-only semantics, so no current config changes behavior.
- **Name the keys `max_utilization_*`.** "Limit" already means the upstream quota itself in this codebase ("Fable-scoped weekly limit") and also the spend limits under `[server.spend]`. "Utilization" matches the 0.0–1.0 fraction the value holds and keeps the new keys distinct from `threshold`/`hard_threshold`.
- **Cap Fable's own bucket rather than reserve shared headroom for Opus.** The user chose a cap on total Fable consumption. Keeping shared-window headroom for non-Fable models is a different policy and is deferred (see Scope Boundaries).
- **Exclude at selection time, not through a cooldown.** Each selection evaluates the account's current utilization. A cap change or a window reset therefore takes effect immediately. Policy exclusion also stays distinct from failure cooldowns.
- **Pool-wide exhaustion follows the existing failover chain.** A cap limits use of this pool, not of the model. If the route has a later upstream, capped traffic moves there, even when that upstream is pay-per-use.
- **Cover all three windows.** Caps for 5h and 7d ship alongside the Fable cap. The public key shape then matches the existing `threshold_5h`/`_7d`/`_fable` family and will not need reshaping later.

---

## Requirements

**Configuration**

- R1. Each account accepts `max_utilization`, `max_utilization_5h`, `max_utilization_7d`, and `max_utilization_fable`.
- R2. `[server.pool]` accepts `default_max_utilization`, `default_max_utilization_5h`, `default_max_utilization_7d`, and `default_max_utilization_fable`.
- R3. For each window, the effective cap resolves in this order: account window key → account `max_utilization` → pool window default → pool `default_max_utilization` → no cap.
- R4. With no cap configured anywhere, selection behaves exactly as it does today.
- R5. Cap values are utilization fractions in `[0.0, 1.0]`. An out-of-range value fails startup and `shunt check`, the same way threshold validation does today.

**Selection**

- R6. A request excludes an account when the account's observed utilization in any window that governs the request is at or above that window's effective cap.
- R7. 5h and 7d caps govern every request. The Fable cap governs Fable-model requests only, and only once the account's `7d_oi` utilization has been observed.
- R8. A window with no observed utilization never causes exclusion.
- R9. An excluded account becomes eligible again with no operator action once its utilization falls below the cap, whether through a window reset, a lower observation, or a raised cap.
- R10. The cap stops new routing; it is not a precise budget. In-flight requests are not cancelled, and concurrent requests may push utilization past the cap by whatever they consume: nothing reserves usage for requests already dispatched, so the overshoot is bounded only by the concurrent load, not by a small margin.
- R11. Accounts that are not excluded keep today's ordering. Priority, soft thresholds, burn-rate avoidance, `sort_by_reset`, and cooldowns all apply unchanged.

**Pool exhaustion**

- R12. When no account in the pool is selectable for a request, and at least one account was excluded by a cap, the pool returns a gateway-owned rate-limit error. The error names the cap as the cause and gives the earliest known time an account becomes eligible.
- R13. That error advances the `[[upstreams]]` failover chain the same way an upstream 429 does. The client sees it only when no later upstream serves the request.
- R14. The error uses the existing gateway error shape: Anthropic everywhere, except the inbound Codex endpoint, which uses the OpenAI Responses shape.

**Providers**

- R15. Caps apply to every pool that uses the shared account-pool selection: Claude OAuth, Codex/ChatGPT, and Kimi. A pool that reports no data for a window never excludes on that window (per R8).
- R16. On Codex/ChatGPT pools, the 5h and 7d caps govern Codex's 5-hour and weekly windows. `max_utilization_fable` has no effect there.

**Observability**

- R17. Every status surface that reports whether an account is selectable shows a capped account as capped. This covers the admin dashboard, `GET /usage`, and the `/api/oauth/usage` synthesizer. "Capped" is a distinct state from cooling, paused, and disabled, and a Fable-only cap shows as Fable-scoped.

---

## Acceptance Examples

- AE1. Fable cap leaves Opus untouched
  - **Covers:** R6, R7
  - **Given:** Account A has `max_utilization_fable = 0.5` and an observed `7d_oi` utilization of 0.55. Its 5h and 7d windows are under their caps.
  - **When:** A Fable request and an Opus request arrive.
  - **Then:** The Fable request skips A. The Opus request may select A.
- AE2. Whole pool capped, with and without a later upstream
  - **Covers:** R12, R13
  - **Given:** Every account in the pool is past its Fable cap.
  - **When:** A Fable request arrives on a route with a second upstream.
  - **Then:** The second upstream serves it. On a route with no second upstream, the client receives a rate-limit error that names the cap and the earliest eligibility time.
- AE3. Unobserved window
  - **Covers:** R8, R10
  - **Given:** A newly added account has no `7d_oi` observation and `max_utilization_fable = 0.5`.
  - **When:** A Fable request arrives.
  - **Then:** The account is eligible. Once a response reports `7d_oi` utilization of 0.6, the next Fable request skips it.
- AE4. Shared-window cap blocks every model
  - **Covers:** R7, R9
  - **Given:** An account has `max_utilization_5h = 0.8` and an observed 5h utilization of 0.85.
  - **When:** Opus and Fable requests arrive.
  - **Then:** Both skip the account until its 5h utilization drops below 0.8.
- AE5. Soft threshold and hard cap together
  - **Covers:** R11
  - **Given:** An account has `threshold_fable = 0.4` and `max_utilization_fable = 0.6`.
  - **When:** Its `7d_oi` utilization is 0.45, and later 0.6.
  - **Then:** At 0.45 the account sorts behind under-threshold peers but stays selectable. At 0.6 Fable requests exclude it.
- AE6. Fable cap on a Codex account
  - **Covers:** R16
  - **Given:** A Codex account sets `max_utilization_fable = 0.1`.
  - **When:** Any request arrives.
  - **Then:** Selection is unaffected.

---

## Scope Boundaries

- Rewriting a capped Fable request to Opus automatically.
- Editing caps at runtime from the admin dashboard or admin API.
- Fable budgets per user or per client key.
- Reserving shared-window headroom for non-Fable models, that is, a cap on 5h/7d that applies only to Fable requests.
- Excluding accounts based on burn-rate projection rather than observed utilization.
- Pool-aggregate caps, such as "Fable may use at most X across the whole pool."
- Bypassing failover for cap-driven errors.

---

## Dependencies / Assumptions

- Fable usage probably also counts toward the shared `7d` window, given Claude Code's "Current week (all models)" label. This is unverified. If it holds, a Fable cap bounds Fable's drain on Opus's weekly budget only indirectly.
- R9 assumes observed utilization for a window stops counting once that window's reset time passes. This was not verified. Planning must confirm it or add expiry.
- An upstream `rejected` window status already marks an account near-quota even without a utilization value (`assess_quota` in `src/accounts.rs`). Caps act on utilization only. Rejection keeps its current handling.

---

## Outstanding Questions

### Deferred to Planning

- The exact error status and message, and whether the error carries a `retry-after`. The status must be one that the failover chain advances on.
- How validation treats a cap below the resolved soft threshold or above `hard_threshold`: reject, warn, or accept.
- How a capped sticky-session account hands off. Moving the session costs its prompt-cache locality.
- Whether a cap change applies at runtime depends on how shunt loads config today.
- Which per-window cap fields the admin snapshot needs in order to render R17.

---

## Sources / Research

- `src/accounts.rs` → `resolved_threshold`, the precedence chain R3 mirrors.
- `src/accounts.rs` → `assess_quota`: governing windows, Fable `7d_oi` selection, and rejection handling.
- `src/accounts.rs` → `select_order_inner`, ordering buckets (`available_under` → `near_soft` → `over_hard` → `cooled`). Nothing is excluded for quota today.
- `src/accounts.rs` → `select_order_inner`, rotation step: it filters only `disabled` and runtime pause. Pause is memory-only.
- `src/config.rs` → `PoolConfig` and `AccountConfig`: threshold fields the new keys sit beside.
- `src/proxy/failover/chain.rs` → `is_advance_status`: a 429 advances to the next upstream.
- `src/adapters/anthropic/mod.rs` → `forward_claude_oauth`, the dedicated all-disabled pool error, prior art for R12.
- `src/adapters/responses/pool.rs` — Codex/ChatGPT pools share selection and have no Fable window.
- `ui/src/accounts.ts` → `managedState`, the existing `cooling` / `cooling-fable` split that R17's capped state parallels.
- PR #329 (Fable `7d_oi` split) — making availability model-scoped silently desynced the dashboard and the usage synthesizer. R17 exists to prevent a repeat.
- `site/src/content/docs/reference/configuration.md:400` — the `[server.pool]` reference the new keys extend. Its locale copies must move with it.
