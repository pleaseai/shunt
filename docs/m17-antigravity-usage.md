# M17 — Antigravity pool usage reporting

## What this is

Managed `antigravity_oauth` accounts already appeared in the admin dashboard,
but their Usage column had no provider-native quota signal. This milestone adds a
third arm to the background usage poller so each refreshable Antigravity pool
account can report its subscription quota without depending on a running IDE
process.

## The quota source

The poller calls:

`POST {normalized_base}/v1internal:retrieveUserQuotaSummary`

with the account OAuth bearer, the Antigravity Hub `User-Agent`, and the
account's project id in the JSON body. Production
`cloudcode-pa.googleapis.com` configurations are normalized through
`auth::antigravity::auth::inference_base_url`, so quota polling uses
`daily-cloudcode-pa.googleapis.com` just like Antigravity inference and model
catalog traffic. Loopback/custom proxy hosts are left unchanged.

This is a private, observed API rather than a public Google integration
contract. Failures are therefore fail-soft: a bad response leaves the previous
good dashboard snapshot in place.

## Quota semantics

Antigravity does **not** expose one independent subscription budget per catalog
model. The summary response groups quota into two shared model-family pools:

- **Gemini Models**
- **Claude + GPT Models** (the `3p` bucket family)

The provider currently exposes 5-hour and weekly windows for those groups when
the account tier supports them. Canonical bucket ids are:

- `gemini-5h`
- `gemini-weekly`
- `3p-5h`
- `3p-weekly`

Each usable bucket carries its own `remainingFraction` and optional
`resetTime`. Free or otherwise restricted tiers may omit windows; Shunt simply
shows the usable windows the summary actually returns.

The parser accepts the canonical ids first and has a narrow group-name/window
fallback for cosmetic upstream id changes. Unknown groups/windows are ignored,
and a response with no recognized numeric remaining fraction is rejected so it
cannot replace a last-known-good snapshot.

## Pool state and dashboard

The grouped windows are flattened into the existing dashboard
`quota_buckets` shape, with labels such as:

- `Gemini Models · 5h`
- `Gemini Models · weekly`
- `Claude + GPT Models · 5h`
- `Claude + GPT Models · weekly`

They are **display-only**. `AccountPool::note_antigravity_usage` updates
`health.quota_buckets` but leaves the generic `health.quota` selection state
untouched, so Antigravity account selection/rotation behavior does not change.
The buckets are memory-only and are re-fetched by the poller; no
`state_persist.rs` migration is required.

This boundary matters because quota-aware routing for Antigravity would need to
be model-family aware: Gemini requests must consult the Gemini pool, while
Claude/GPT requests must consult the other-model pool. Folding either group into
the generic single 5h/7d state would be incorrect.

## Eligibility

Only imported (refreshable) Antigravity logins are polled. `token_env`
credentials are treated as static, and a credential file must contain a
non-empty top-level `refresh_token`. Credential-file eligibility deserializes
only that field rather than building a generic JSON value.

## Operator note

The quota-summary endpoint and response shape are private implementation details
and may change upstream. Enabling `[server.pool] usage_refresh_seconds` adds one
best-effort metadata poll per eligible account on each interval.
