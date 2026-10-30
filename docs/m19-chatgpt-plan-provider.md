# M19 — ChatGPT plan provider (Sign in with ChatGPT) (spec)

> Companion to [`m18-xai-multi-account.md`](m18-xai-multi-account.md) (the named-account store and
> pool this follows), [`m10-codex-multi-account.md`](m10-codex-multi-account.md) (the Responses
> pool this rides), [`m1-responses-translation.md`](m1-responses-translation.md) (the translation it
> reuses), and [`notes/chatgpt-plan.md`](notes/chatgpt-plan.md) (the live probe behind the request
> rules). The decision record is
> [ADR-0007](../.please/docs/decisions/0007-chatgpt-plan-provider.md).

## 1. Scope

- Serve mapped model ids from a ChatGPT plan through OpenAI's "Sign in with ChatGPT" (SIWC)
  token-sharing preview, on `https://api.openai.com/v1/responses`. This is separate from the
  `codex` provider, which reuses the Codex CLI login against the ChatGPT backend.
- A new preset and provider name, `chatgpt-plan` (`kind = "responses"`), and a new auth mode,
  `chatgpt_plan_oauth`. No existing key changes meaning.
- Two commands: `shunt login chatgpt-plan --name <account>` and
  `shunt logout chatgpt-plan --name <account>`.
- One environment variable for users: `SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR`, which overrides the
  account directory.
- Out of scope: admin-surface provisioning, upstream model discovery (shunt serves the ids you
  map), namespaced tools, the Responses WebSocket transport, session handoff between hosts, and any
  change to the `codex` provider.

## 2. Account store

Each named account is one file:

```text
~/.shunt/accounts/chatgpt-plan/<name>.json
```

Override the directory with `SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR` (`src/auth/chatgpt_plan/store.rs`).
Each file is written `0600` in a `0700` directory, atomically, through the shared account-file
writer. Two names that share an email address keep two separate records and client ids.

A signed-in file holds `version`, the issued `client_id`, the validated `sub` and `email`, the granted
`scopes`, and a `tokens` block (`access_token`, `refresh_token`, `id_token`, `expires_at`,
`earliest_refresh_at`, `last_refresh`). A signed-out file is exactly `{version, client_id}`:
the registration survives so the next sign-in reuses the issued client id, and the pool resolves the
account as needing sign-in.

The installation's host identifier lives beside the accounts, in `.host-id`, as `urn:uuid:<v4>`. It
is created once, before the first sign-in, and never scanned as an account. Login, refresh
writeback, and logout take a cross-process file lock, because the refresh token rotates and the CLI
can run beside the gateway.

## 3. Sign-in

`shunt login chatgpt-plan --name <account>`:

1. Reads the account's saved client id, if any, and the host id.
2. Binds `127.0.0.1:1455` (the redirect the registration requires) and prints the authorize URL,
   opening the browser when it can. If the port is taken, it prints a notice and continues in
   paste-only mode.
3. Waits for either the loopback callback or a callback URL pasted into the terminal, whichever
   arrives first. A pasted URL may carry surrounding whitespace or quotes. The state must match this
   attempt before the code is exchanged. The wait times out after five minutes.
4. Exchanges the code with the PKCE verifier and validates the ID token (RS256 against OpenAI's
   published keys; issuer, audience equal to the issued client id, expiry with a bounded clock
   skew, and this attempt's nonce).
5. Requires the `chatgpt.tokens.use.direct` scope. Without it the account keeps only its client id
   and the command reports that plan usage was not granted.
6. Writes the account file only after every check passes.

The first sign-in sends `client_id=dynamic_agent_client` and `agent_name_hint=shunt` and stores the
issued `oaiapp_…` id from the callback. Later sign-ins send the saved id and no hint. A callback that
names a different client id is rejected and the file is left unchanged; a first sign-in whose
callback carries no issued id reports the registration as incomplete and stores nothing.

`--long-lived`, `--mode`, and `--manual` are rejected: plan tokens are always refreshable, and the
paste path is always available.

## 4. Configuration

```toml
[providers.chatgpt-plan]
kind = "responses"
base_url = "https://api.openai.com/v1"   # shunt appends /responses
auth = "chatgpt_plan_oauth"

[[routes]]
model = "gpt-6.1-sol"
provider = "chatgpt-plan"
```

In an ordered `[[upstreams]]` list, `provider = "chatgpt-plan"` fills in the same three keys.
Select accounts with `[[providers.chatgpt-plan.accounts]]` entries (`name`, optional `priority`) or
leave them out to use every account in the store.

Validation (`shunt check`) rejects, naming the offending key:

- a `chatgpt_plan_oauth` provider whose `kind` is not `responses`;
- a `base_url` that is plaintext `http` on a non-loopback host;
- a `base_url` host other than `api.openai.com` (loopback hosts are allowed for local mocks);
- a provider named `chatgpt-plan` with any other `auth`.

Native `tool_search` resolves off for this auth mode, whatever the `tool_search` setting says, because
the preview rejects the tool. Hosted `web_search` passes through the existing translation unchanged.

## 5. Request shaping

The request is the existing OpenAI-dialect Responses translation, which the probe showed the
preview accepts unchanged, with these fixed rules:

- `temperature`, `metadata`, and `prompt_cache_retention` are never sent.
- `store: false` and `stream: true` are always sent. A client that asked for a non-streaming reply
  still gets one complete Anthropic message, assembled from the upstream stream.
- `max_tokens` is forwarded as `max_output_tokens`, behind one code constant in
  `src/adapters/responses/pool_plan.rs` that stops forwarding if OpenAI starts enforcing its
  documented limitation.
- The bearer is the account's plan access token. No `chatgpt-account-id` or `originator` header is
  sent.

A response capped by `max_output_tokens` ends the Anthropic turn as `end_turn`, not `max_tokens`.
That is the translator's behavior for every Responses provider (§ `response.incomplete` in
`m1-responses-translation.md`); changing it is tracked in
[issue #868](https://github.com/pleaseai/shunt/issues/868).

## 6. Failover

The account pool uses the Responses pool loop shared with Codex and xAI. A session
(`x-claude-code-session-id`) stays on one account; otherwise accounts rotate. The serving account is
named in the `x-shunt-account` response header.

| Upstream answer | Pool action |
| :-- | :-- |
| 429, or in-stream `subscription_sharing_usage_limit_exceeded` before any output | Cool the account (the `Retry-After` value, or 60 s) and serve the request from the next account. |
| 5xx, `subscription_sharing_usage_unavailable`, `subscription_sharing_user_unavailable` | Cool the account for 30 s, keep its credentials, and try the next account. When none is left, return `overloaded_error`. |
| 401, `subscription_sharing_invalid_user` | Force one refresh of that account's token and retry once. If the refresh fails terminally, mark the account as needing sign-in and cool it for five minutes. |
| 403 (`user_not_eligible`, `route_not_supported`, `chatpass_v2_*`) | Relay as `permission_error`. No retry, no refresh, no sign-in. |
| Any other 4xx | Relay as is. |

When every account is cooling down for its usage limit, the gateway returns a `rate_limit_error`
that points to <https://chatgpt.com/settings/usage>. A 401 that ends in a failed refresh points to
`shunt login chatgpt-plan --name <account>`.

### 6.1 In-stream failures

The preview reports some failures as a `response.failed` or `error` event on a `200 OK` stream. For a
plan account the pool holds only lifecycle frames (`response.created`, `response.in_progress`,
`response.queued`), up to 64 KiB, until the first other frame arrives:

- if that frame is a mapped failure, the pool treats it like the matching HTTP status and rotates;
- otherwise it replays the held bytes unchanged and relays the rest of the stream as it arrives.

A held lifecycle frame carries no model output, so streaming is not delayed for the client.

Two cases are not rotated:

- A usage limit that arrives **after** output has reached the client ends the client stream with an
  Anthropic `rate_limit_error` event instead of a normal message stop, and the account is cooled as
  an HTTP 429 would cool it. The turn cannot move to another account once part of it has been sent.
- An **idle-gated** call (the [escalation](../site/src/content/docs/guides/escalation.mdx) and
  [advisor](../site/src/content/docs/guides/advisor.mdx) calls, which run under an idle bound) skips
  the pre-content check so the read cannot sit outside that bound. A usage limit that arrives
  in-stream on such a call ends it with the mapped error and does not rotate. The account is still
  cooled.

## 7. Refresh

An access token is renewed once it is within five minutes of expiry, and never before the server's
`earliest_refresh_at`. The refresh sends the issued client id and the API resource and persists the
replacement refresh token under the file lock. Concurrent and cancelled callers trigger one POST
and never lose a rotated token. A refresh that fails with `invalid_grant` or `invalid_client`, or an
account file that is signed out or corrupt, is terminal: the account is marked as needing sign-in
and cooled down so healthy accounts serve first. A transient failure (network, 5xx, 429) cools the
account without marking it.

A refresh that finishes after `shunt logout` has cleared the account discards its result, so a
sign-out is never undone.

## 8. Sign-out

`shunt logout chatgpt-plan --name <account>` revokes the refresh token at the revocation endpoint
that OpenAI's discovery document advertises (https, issuer host only), then clears the access,
refresh, and ID tokens. It keeps the issued client id and the host id.

| Outcome | Message |
| :-- | :-- |
| HTTP 200 (also for an already-invalid token) | `ChatGPT plan account "<name>" signed out; remote revocation confirmed.` |
| Network or 5xx failure after three attempts with backoff, or no usable discovery document | `ChatGPT plan account "<name>" signed out locally; remote revocation not confirmed — finish in ChatGPT settings.` The tokens are cleared locally anyway. |
| The account has no tokens | `ChatGPT plan account "<name>" is already signed out.` |

A 4xx answer from the revocation endpoint is not retried.

## 9. Error mapping

`src/model/siwc_errors.rs` is the one table. It serves both the HTTP error path and the in-stream
event path, and appends `(parameter: <param>)` to the message when the payload names one.

| Code | Status | Anthropic type |
| :-- | :-- | :-- |
| `subscription_sharing_usage_limit_exceeded` | 429 | `rate_limit_error` |
| `subscription_sharing_usage_unavailable`, `subscription_sharing_user_unavailable` | 503 | `overloaded_error` |
| `subscription_sharing_unsupported_capability` | 400 | `invalid_request_error` |
| `subscription_sharing_user_not_eligible`, `subscription_sharing_route_not_supported`, any `chatpass_v2_*` | 403 | `permission_error` |
| `subscription_sharing_invalid_user` | 401 | `authentication_error` |
| A `{"detail": "…"}` parameter rejection | 400 | `invalid_request_error`, carrying the upstream message |

A code outside the table maps by its HTTP status, like other providers' errors. Gateway-owned
errors keep the Anthropic error shape.

## 10. Not covered

- Quota-aware selection and usage polling: the pool reacts to the usage-limit answer; it does not
  read plan usage ahead of time.
- The `max_tokens` stop reason for a capped turn (issue #868).
- A public config key for the `max_output_tokens` switch.
- Provisioning accounts from the admin web surface.
