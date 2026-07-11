# M7 — Account load balancing (design sketch)

> **⚠️ Proposal — not implemented.** This is a forward-looking design sketch, not a shipped
> feature. It records the intended shape so the credential layer stays load-balancing-ready
> and so the trade-offs (prompt-cache locality, subscription ToS) are decided deliberately
> rather than by accident. Config keys, strategy names, and state model may change once built.

> Companion to [`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md), [`m6-xai-provider.md`](m6-xai-provider.md),
> and the Cursor provider. Adds an **account pool** below a provider: shunt selects one
> credential per request from a pool of same-provider accounts (e.g. 3 Claude subscriptions,
> 2 Codex/ChatGPT logins), with session affinity and per-account failover.

## 1. Scope

- A **credential pool** per provider: instead of one credential source, a provider may declare
  N accounts, each with its own credential file/env.
- A **selection strategy** applied per request: session-affinity first, health-aware failover
  second.
- **Per-account health state**: track `429` / quota exhaustion and honor each account's
  `Retry-After` independently (cool an account down, skip it until its timer clears).

Out of scope: cross-provider balancing (a Claude request never falls over to Codex — different
model families and wire protocols); any change to routing (model → provider stays 1:1); any
change to the adapters, the M1 translation core, or the SSE machine.

## 2. Motivation

Two forces make single-account injection a bottleneck for anything beyond personal use:

1. **Per-account rate limits.** One subscription (Claude / ChatGPT / Cursor / SuperGrok) has a
   fixed request/token budget. A shared gateway ([`m4-inbound-auth.md`](m4-inbound-auth.md))
   fanning many clients onto one account hits that ceiling quickly. Pooling N accounts raises
   aggregate throughput ~N× and lets a `429`'d account step aside instead of failing the request.

2. **Prompt caching is per-account — so naive round-robin is actively harmful.** Anthropic and
   OpenAI prompt caches are scoped to the credential. Bouncing a *single conversation* across
   accounts per request shreds cache hits mid-stream → higher cost and latency, not just
   no-gain. **Session affinity is therefore a correctness concern, not just an optimization.**

The design goal: pool for capacity **without** breaking cache locality.

## 3. Selection strategy (affinity-first, failover-second)

Per request, pick an account in this order:

1. **Sticky by session.** shunt already threads `x-claude-code-session-id` through
   [`proxy.rs`](../src/proxy.rs). Pin a session to an account (stable hash → account, or a
   session→account map) and keep it there for the whole conversation. This preserves prompt
   cache locality.
2. **Failover on unavailability.** If the pinned account is cooled down (recent `429` / quota
   exhaustion, see §5), pick the next healthy account by the fallback policy and **re-pin** the
   session to it.
3. **Fallback policy for new/unpinned sessions:** `least_loaded` (fewest in-flight / lowest
   recent error rate) by default; `round_robin` and `weighted` as alternatives.

Requests with no session id (rare from Claude Code; possible from other clients) fall straight
to the fallback policy with no stickiness.

## 4. Config shape (table-driven — AGENTS.md rule)

Keep routing untouched; the pool lives **below** the provider. A provider gains an optional
`accounts` list and a `strategy`. When `accounts` is absent, behavior is exactly today's
single-credential path (fully backward compatible).

```toml
[providers.claude]
kind = "anthropic"
auth = "claude_oauth"
strategy = "session_sticky"          # then least_loaded | round_robin | weighted as fallback

  [[providers.claude.accounts]]
  auth_file = "~/.shunt/claude-1.json"

  [[providers.claude.accounts]]
  auth_file = "~/.shunt/claude-2.json"
  weight = 2                         # weighted fallback only

  [[providers.claude.accounts]]
  auth_file = "~/.shunt/claude-3.json"

[providers.codex]
kind = "responses"
auth = "chatgpt_oauth"
strategy = "session_sticky"

  [[providers.codex.accounts]]
  auth_file = "~/.codex/auth-a.json"

  [[providers.codex.accounts]]
  auth_file = "~/.codex/auth-b.json"
```

- Each account entry names its own credential source (`auth_file`, or `token_env` for API-key /
  static-token providers). The provider's `auth` mode still selects the store type; the account
  entry only overrides *where* that store reads from.
- Validation: `accounts` requires ≥1 entry; `weight` only meaningful for `weighted`; a mixed
  pool (some `auth_file`, some `token_env`) is allowed as long as each resolves under the
  provider's `auth` mode.

## 5. Credential resolution seam

The whole feature lands behind one existing function:
[`resolve_credential(config, route, client)`](../src/auth/mod.rs). Today it returns **one**
`Credential`. Load balancing changes it to **select** from the pool:

```
resolve_credential(config, route, client, session_id, account_state)
  -> (Credential, AccountKey)
```

- Adapters, routing, and proxy are untouched — they already call this one function.
- `Credential` variants gain (or the return tuple carries) an **`AccountKey`** so metrics,
  health state, and session stickiness can all key off the same identity. This is a small,
  additive change; the Cursor / xai / codex stores each already encapsulate one account, so a
  pool is just N of them behind a selector.

## 6. Per-account health state

A small concurrent map keyed by `AccountKey`, held alongside the hot-swappable `RuntimeState`
in [`reload.rs`](../src/reload.rs):

| Field | Purpose |
| :-- | :-- |
| `cooldown_until` | set from `Retry-After` on a `429`; account skipped until it passes |
| `in_flight` | live request count (drives `least_loaded`) |
| `recent_errors` | rolling error signal (drives health / eviction) |

- The gateway already extracts `Retry-After` per response and re-shapes upstream errors; wire
  those signals into the per-account state rather than only the client-facing error.
- On reload, the pool membership is rebuilt from config; health state for surviving accounts is
  preserved where possible.

## 7. Interaction with existing features

- **Inbound auth ([m4](m4-inbound-auth.md)):** orthogonal. Inbound tokens authenticate the
  *client*; account pooling picks the *upstream* credential. A shared gateway uses both.
- **Discovery ([m3](m3-discovery.md)) & routing:** unchanged — still one provider per model.
- **Metrics:** extend the per-provider/model series with an `account` dimension so per-account
  load and error rates are observable (needed to tune `weighted` / `least_loaded`).
- **`shunt login`:** each account is acquired by the same one-time login flow, just written to a
  distinct file (`shunt login cursor --out ~/.shunt/cursor-2.json`, or per-account env). No new
  auth mechanism.

## 8. The subscription-ToS caveat (decide deliberately)

Pooling solves the **technical** per-account limit. It does **not** resolve the **policy**
question: pooling personal subscription accounts to serve a multi-user gateway is exactly the
pattern most subscription terms (Claude, ChatGPT, Cursor) push back on. API-key providers
(`XAI_API_KEY`, `OPENAI_API_KEY`, Anthropic keys) carry no such tension and are the intended
home for real multi-user capacity. This doc enables the mechanism; **whether to point it at
subscription logins for a shared service is an operator decision that should be made with eyes
open**, and the docs should say so.

## 9. Open questions

- Session→account pinning: stable hash (stateless, survives restarts) vs. an explicit map
  (handles rebalancing / eviction, but is state to reload). Start with stable hash + failover
  re-pin.
- Eviction: when does a persistently-failing account leave the pool, and how does it return?
- Fairness vs. cache locality: `least_loaded` can fight stickiness under load — affinity must
  win unless the pinned account is cooled down.
- Do we need per-account concurrency caps (some subscriptions throttle parallel requests)?
