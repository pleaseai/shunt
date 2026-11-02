# ADR-0007: ChatGPT plan provider as its own auth mode on the OpenAi dialect, with a pre-content usage-limit check

## Status

Accepted

## Date

2026-10-30

## Context

OpenAI's "Sign in with ChatGPT" (SIWC) token-sharing preview lets an open-source tool spend a
user's ChatGPT plan usage on the public `POST https://api.openai.com/v1/responses` endpoint. The
tool registers itself on first sign-in (`client_id=dynamic_agent_client`), receives an issued
`oaiapp_…` client id, and uses a bearer token scoped `chatgpt.tokens.use.direct` for
`resource=https://api.openai.com/v1`.

shunt already has a `codex` provider (`auth = "chatgpt_oauth"`) that reuses the Codex CLI's
login against `chatgpt.com/backend-api/codex/responses`. The SIWC token is a different grant: a
different client, token endpoint, audience, and host, no `chatgpt_account_id` claim, and an
endpoint that speaks the public Responses dialect rather than the ChatGPT backend one.

A live probe (2026-10-30, track `chatgpt-plan-provider-20261030`) sent shunt's own translation of a
Claude Code request with tools to the preview. The `OpenAi`-flavor body was accepted unchanged —
plain function tools, `reasoning`, `text`, `include`, `max_output_tokens`, `prompt_cache_key`.
Only parameters shunt does not emit for that flavor (`temperature`, `metadata`,
`prompt_cache_retention`), the wrong `store`/`stream` values, and a `tool_search` tool were
rejected. The preview supports hosted `web_search`.

The preview reports a plan usage limit (`subscription_sharing_usage_limit_exceeded`) either as an
HTTP 429 or as a `response.failed` event after the stream has started. shunt's pool rotates on
HTTP status only; an in-stream failure is terminal and never cools an account
(`src/model/responses.rs`).

## Decision

1. **A new auth mode, not a variant of `chatgpt_oauth`.** `auth = "chatgpt_plan_oauth"` with its
   own `StoreFamily::ChatgptPlan`, `Credential` variant (bearer only), account store under
   `~/.shunt/accounts/chatgpt-plan/`, and a `chatgpt-plan` preset. The codex store, flavor, and
   headers stay untouched.
2. **Reuse the `OpenAi` Responses dialect.** No new `ResponsesFlavor`. The api.openai.com host
   already selects `OpenAi`; the only dialect gate added is that native `tool_search` stays off
   for this auth mode. Hosted `web_search` stays available. Forwarding `max_output_tokens` sits
   behind one code constant so it can be turned off without a config key if OpenAI starts
   enforcing its published limitation.
3. **Classify a pre-content in-stream failure like an HTTP status.** For this store family only, on
   a 2xx upstream response the pool reads the stream up to its first non-lifecycle event
   (`response.created` / `in_progress` / `queued` are held). If that event is a `response.failed`
   or `error` whose code the SIWC error table maps to a status, the pool continues with a
   synthetic response of that status; otherwise it replays the held bytes and relays the rest
   untouched. The existing classification then rotates 429/5xx, refreshes once on 401, and relays
   other 4xx — in both the streaming and non-streaming pool loops.
4. **One SIWC error table** (`subscription_sharing_*`, `chatpass_v2_*` → status and Anthropic
   type) feeds both the HTTP and in-stream mappers.
5. **Reuse `auth::inbound_jwt::verify`** for the ID token (issuer, audience = issued client id,
   RS256, nonce via `require`, 60 s clock-skew leeway); `inbound_jwt.rs` itself is not changed.

## Consequences

- Claude Code users can divert mapped model ids to their ChatGPT plan through the sanctioned SIWC
  path, with named-account pooling and usage-limit failover.
- New public config values (`chatgpt_plan_oauth`, preset `chatgpt-plan`) and a new top-level
  `shunt logout` command. No existing key or credential file changes behavior.
- Holding only lifecycle frames keeps streaming semantics: they carry no model output and are
  invisible to the Anthropic client, which has already received its synthetic `message_start`.
- A usage limit that arrives after output has started is not rotated; it ends the client stream
  with a `rate_limit_error` event and cools the account.
- On idle-gated calls (`routing::serve`), the peek is skipped so it cannot sit outside the idle
  bound; those calls fall back to the terminal in-stream error.
- About 300 lines of the xAI refresh store are adapted rather than generalized; a shared
  refresh-store abstraction is deferred until a fourth pooled OAuth family arrives.

## Alternatives considered

- **Extend `chatgpt_oauth` with a mode switch.** Rejected: the codex store requires
  `chatgpt_account_id`, which plan tokens lack, and any change there risks the codex provider.
- **A new `ResponsesFlavor::ChatgptPlan`.** Rejected: the probe showed `OpenAi` is already the
  accepted dialect, and a flavor touches every flavor match for no behavioral difference.
- **Peek inside `pool_events_stream`'s relay phase.** Rejected: covers only the streaming loop,
  needs duplicating in the non-streaming and refresh-retry arms, and sits on the
  latency-critical relay path.
- **Hard-exclude accounts needing sign-in.** Rejected for this track: it changes shared pool
  selection used by codex and xAI; the existing cooldown + `needs_relogin` flag applies instead.

## Related

- Track `chatgpt-plan-provider-20261030`: [spec](../tracks/completed/chatgpt-plan-provider-20261030/spec.md), [plan](../tracks/completed/chatgpt-plan-provider-20261030/plan.md)
