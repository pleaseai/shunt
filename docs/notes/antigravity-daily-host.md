# Antigravity: the daily backend host and effort-suffixed model ids

## Overview

Every request the built-in `antigravity` provider sent to
`cloudcode-pa.googleapis.com` failed with:

```json
{
  "error": {
    "code": 429,
    "message": "Resource has been exhausted (e.g. check quota).",
    "status": "RESOURCE_EXHAUSTED"
  }
}
```

The message reads as a rate limit. It is not one. Two separate defects
produced it, and both were reproduced against a live Antigravity token.

## Probe matrix

| host | model | envelope | result |
| --- | --- | --- | --- |
| `cloudcode-pa.googleapis.com` (old default) | `gemini-3.8-flash` | shunt's `{model, project, request}` | 429 "check quota" |
| `cloudcode-pa.googleapis.com` | `gemini-3.8-flash-medium` | shunt's | 503 |
| `daily-cloudcode-pa.googleapis.com` | `gemini-3.8-flash-medium` | plus `userAgent`, `requestType`, `requestId`, `request.sessionId` | **200** |
| `daily-cloudcode-pa.googleapis.com` | `gemini-3.8-flash` | full envelope | 404 `NOT_FOUND` |

Read together: on the daily host a **bare model slug does not exist** —
that row varied only the id, so the `404` is attributable to it. The
production `429` is *not*: that request differed from the working one in
both the model id (bare) and the envelope (plain Code Assist), so the
probes do not establish which input produced it. What the matrix does
establish is the working combination — suffixed id plus the full envelope
on the daily host — not a cause for each individual failure.

## The catalog

The Antigravity client is served effort-suffixed Gemini ids:

- `gemini-3.8-flash-{high,medium,low}`
- `gemini-3.7-flash-{high,medium,low}`
- `gemini-3.6-flash-{high,medium,low}`
- `gemini-3.1-pro-{high,low}` — Pro has **no** `-medium`
- `claude-sonnet-4-6`, `claude-opus-4-6-thinking`, `gpt-oss-120b-medium`
  (not Gemini, no suffix synthesis)

`agy models` lists the catalog as it currently stands. Note that other
proxy implementations query `fetchAvailableModels` on more than one host,
so "only the daily host serves this catalog" is *not* established here —
what is established is that the `agy` client addresses the daily host for
both discovery and inference.

## Reference implementation

router-for-me/CLIProxyAPI,
`internal/runtime/executor/antigravity_executor_request.go`:

- `resolveAntigravityRequestBaseURL` sends inference to the `daily-` host.
- `geminiToAntigravity` adds `userAgent: "antigravity"`,
  `requestType: "agent"`, `requestId: "agent-<uuid v4>"`, and
  `request.sessionId: "-<up to 19 decimal digits>"`.
- `generateStableSessionID` derives the session id from the conversation
  rather than drawing it at random, so follow-up turns stay in one
  session.

The `agy` CLI itself also calls `loadCodeAssist` and
`fetchAvailableModels` on the daily host, so the daily host is the right
target for discovery as well as inference.

## Resulting shunt behaviour

**Host.** The built-in `antigravity` provider is seeded at
`https://daily-cloudcode-pa.googleapis.com`, and
`shunt login antigravity` falls back to the same host when no config
loads, so login provisions its project against the backend inference
uses. The `gemini` (Code Assist) provider stays on
`cloudcode-pa.googleapis.com`.

The `antigravity_oauth` config guard accepts exactly two non-loopback
hosts — the daily host and production — through its own predicate rather
than borrowing Code Assist's. No other `googleapis.com` host qualifies,
`daily-cloudcode-pa.sandbox.googleapis.com` included, and the
`google_oauth` guard is unchanged. Both onboarding and inference redirect
to the daily control plane for a production-pinned `base_url` — production
serves neither — and startup, `shunt check`, and reload log a warning
naming the provider so the operator can drop `base_url` or point it at the
daily host. Anything in front of the backend (a loopback proxy, or either
host with an explicit port or path prefix) travels with the configured
host instead.

**Envelope.** `AuthMode::AntigravityOauth` requests use
`wrap_antigravity_envelope` instead of `wrap_code_assist_envelope`,
mirroring the identity the Antigravity client sends:

```json
{
  "model": "gemini-3.6-flash-medium",
  "project": "<project id>",
  "userAgent": "antigravity",
  "requestType": "agent",
  "requestId": "agent-<uuid v4>",
  "request": { "...": "...", "sessionId": "-<digits>" }
}
```

The session id is FNV-1a over the earliest non-empty user text part in
the *translated* request — every user turn is scanned, so an image-only
opening turn does not force a random id — rendered as at most 19 decimal digits behind a
leading `-`, bounded by the reference client's `Int63n` modulus
(9e18) so every id fits in a signed 64-bit integer; a request with no
user text falls back to a random value in the same range so such
requests do not all collide on one session. The Code Assist path
sends none of these four fields.

**Model id.** `antigravity_upstream_model` resolves the tier for a bare
`gemini-*` id, taking the first signal that applies:

1. `effort` on the route or provider — an explicit pin.
2. The request's `output_config.effort`.
3. `thinking.type == "enabled"` — `budget_tokens` ≤ 2048 is `low`,
   ≤ 8192 is `medium`, above that is `high`. An enabled block naming no
   budget uses the same `1024` default `translate_thinking_config`
   sends in `thinkingConfig.thinkingBudget`, so it lands in `low` — the
   tier and the budget describe one request rather than two.
4. Otherwise `medium`.

The two effort sources are normalized the same way: Claude Code sends
`low|medium|high|xhigh|max`, and `xhigh` and `max` fold onto `high`
because the catalog stops there. Matching ignores case and surrounding
whitespace, so `High` names the published tier. The sources differ only
in what happens to a level outside that vocabulary. A configured one
passes through trimmed and lower-cased: the catalog, not shunt, decides
which tiers exist, so an operator can name a future one. An unrecognised
request value falls back to `medium` rather than being pasted into the
upstream model id.

A recognised tier is clamped to the tiers the family publishes, so
`medium` on a `-pro` id becomes `high`. An unrecognised configured level
is appended as written and never clamped. The family is read from the
id's `-`-separated segments: `gemini-3-pro-preview` is Pro, and an id
that merely contains the letters is not. Ids that already end in a tier,
and ids that are not `gemini-*`, are sent exactly as written. The
resolved id and the signal that decided it are logged at debug level.
