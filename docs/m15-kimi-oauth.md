# M15 — Kimi Code OAuth provider (spec)

> **⚠️ Not yet live-verified.** Every fact below is grounded in shunt's source and its own test
> suite (mocked against the measured wire shapes described in the code's doc comments), not a real
> device-flow login against a live Kimi Code subscription. Task tracking for that live-verification
> pass is still open; until it lands, treat refresh-token rotation behavior, `anthropic-beta`
> acceptance on `api.kimi.com`, and any latency/quota-window/rate-limit-header behavior as
> unmeasured (see §11).

> Companion to [`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md) (the original subscription-OAuth
> pattern this follows), [`m6-xai-provider.md`](m6-xai-provider.md) (the closest device-code
> analog — RFC 8628, a shunt-owned credential file, single-flight refresh), and
> [`m8-anthropic-multi-account.md`](m8-anthropic-multi-account.md) /
> [`m9-admin-surface.md`](m9-admin-surface.md) (the multi-account pool and admin-surface
> conventions this reuses rather than reinvents). Adds one built-in provider, `kimi-code`, that
> reuses a Kimi Code subscription instead of billing an API key.

## 1. Scope

- A new `AuthMode::KimiOauth` credential kind and a `kimi-code` preset (`kind = "anthropic"`,
  `base_url = "https://api.kimi.com/coding"`, `auth = "kimi_oauth"`) — distinct from the
  pre-existing `kimi` preset (Moonshot's `api_key`-billed `https://api.moonshot.ai/anthropic`).
  Both speak the Anthropic Messages wire shape, so both use the plain `anthropic` adapter kind;
  only the credential source differs.
- `shunt login kimi --name <account-name>`: an RFC 8628 device-authorization login against Kimi's
  own auth service, storing the result as a named, shunt-owned account file rather than a single
  fixed-path credential (multi-account pooling from day one, mirroring Claude/Codex/xAI).
- Multi-account pool integration (`StoreFamily::Kimi`) so `kimi_oauth` providers rotate across
  accounts exactly like `claude_oauth`/`chatgpt_oauth`, and appear in `GET /admin/pool` and
  `GET /usage`.
- Read-only Kimi visibility in the admin web surface (the existing pool dashboard); no
  browser-driven provisioning — accounts are added with the CLI only (§9).

Out of scope: any change to the M1 translation core (Kimi Code speaks native Anthropic Messages,
so no translation is needed), and a live-verification pass against a real subscription (§11).

## 2. OAuth constants

Measured against the live `auth.kimi.com` endpoints (`src/auth/kimi/auth.rs`,
`src/auth/kimi/login.rs`):

| Constant | Value |
| --- | --- |
| Client id | `17e5f671-d194-4dfb-9706-5516cb48c098` (public device-flow client, no secret) |
| Device-authorization endpoint | `https://auth.kimi.com/api/oauth/device_authorization` |
| Token endpoint | `https://auth.kimi.com/api/oauth/token` |
| Device-code grant type | `urn:ietf:params:oauth:grant-type:device_code` |
| Verification URL (from the device-authorization response) | `https://www.kimi.com/code/authorize_device` |
| `expires_in` (device code lifetime) | `1800` seconds, measured; falls back to the same default if a response omits it |
| `interval` (poll interval) | `5` seconds, measured; falls back to the same default if a response omits it, floored at 1s |

The device-authorization endpoint and the human-facing verification URL are different hosts:
shunt POSTs to `auth.kimi.com`, then prints the `verification_uri`/`verification_uri_complete`
the response returns (`www.kimi.com/code/authorize_device[...]`) for the user to open in a
browser on any device.

**Critical wire quirk:** Kimi's token endpoint returns HTTP 400 for the ordinary, non-terminal
`authorization_pending` poll response, and HTTP 400 for a dead refresh token's `invalid_grant`
too — the two are indistinguishable by status code alone. Every request in `src/auth/kimi/`
therefore parses the response body for a token or an OAuth `error` field *before* ever branching
on HTTP status; a status-first implementation (the pattern the module doc comments explicitly
call out as the bug to avoid) would misreport a plain "still waiting" poll as a failure.

## 3. Credential file

Each account is a separate file at `~/.shunt/accounts/kimi/<name>.json`, overridable with
`SHUNT_KIMI_ACCOUNTS_DIR` (`src/auth/kimi/store.rs`). The directory is created `0700`; the file is
written `0600` and atomically replaced on every refresh, preserving every other field (in
particular `deviceId`) it doesn't touch.

```json
{
  "kimiOauth": {
    "accessToken": "...",
    "refreshToken": "...",
    "expiresAt": 1731000000000
  },
  "deviceId": "5c1b7e2e-....(uuid v4)"
}
```

- `expiresAt` is milliseconds since the epoch, computed by shunt at issue/refresh time from the
  response's `expires_in` (falling back to one hour, matching the Claude store's fallback, if a
  response omits it). Kimi's access token is opaque — not a JWT — so expiry is tracked
  out-of-band this way rather than decoded from the token itself.
- `deviceId` is a UUID v4 generated once at `shunt login kimi --name <name>` and persisted for
  the life of the account; it is **not** Kimi Code's own device-id file (shunt does not read that)
  and stays stable across every later refresh and outbound request for that account.
- Unlike the Claude store (`shuntAccountUuid`) or the Codex store (an `account_id`/JWT claim), no
  Kimi Code login response has been observed to carry a stable upstream account identifier — so a
  scanned entry's pool identity is its own file name, the same fallback the Codex store uses for
  its untagged entries.

## 4. Device-code flow (`shunt login kimi --name <account-name>`)

1. Validate the account name (shared validator; same rules as Claude/Codex/xAI: lowercase,
   digits, hyphens).
2. Generate a fresh device id (UUID v4) for this account.
3. `POST` the device-authorization endpoint with `client_id` and the five `X-Msh-*` identity
   headers (§6), parsing the body for `device_code`/`user_code`/`verification_uri` before
   checking status, per §2's wire quirk.
4. Print the verification URL (preferring `verification_uri_complete` when present) and the user
   code; the user approves in a browser on any device.
5. Long-poll the token endpoint (`grant_type=urn:ietf:params:oauth:grant-type:device_code`) at the
   server-supplied interval (default 5s, floored at 1s) until the device-code deadline
   (default 1800s). Each poll response is classified from its parsed body, not its status:
   - `authorization_pending` → keep polling at the current interval.
   - `slow_down` → bump the interval by 5s (capped at 30s) per RFC 8628 §3.5, keep polling.
   - `access_denied` / `authorization_denied` → terminal: "authorization was denied".
   - `expired_token` → terminal, points at re-running login.
   - `invalid_grant` (measured: a bad or reused `device_code`) → terminal, points at re-running
     login — not reported as a bare status code.
   - anything else → terminal, using the response's `error`/`error_description`.
6. On success, persist the access token, refresh token, and computed absolute expiry as the
   account file described in §3. A response with no `refresh_token` at all is rejected — the login
   cannot be persisted without one.

## 5. Token store behavior (`KimiAuthStore::get_valid`, `src/auth/kimi/auth.rs`)

- Reads the account file fresh on every call (off the async runtime, via `spawn_blocking`) and
  returns the stored access token directly if it's valid more than 5 minutes from now (the same
  expiry buffer the Claude store uses).
- Otherwise acquires a single global `tokio::sync::Mutex` (shared across every account and every
  independently-constructed store instance, the same trade-off the Claude and xAI stores already
  make: concurrent refreshes across *different* Kimi accounts serialize behind each other), re-reads
  under the lock in case a concurrent caller already refreshed, and only then calls the token
  endpoint.
- The refresh + writeback runs in a detached `tokio::spawn` task that owns the lock guard, so a
  cancelled caller (e.g. a dropped HTTP request) cannot strand a possibly-consumed refresh token
  mid-flight — the task always finishes the write it started.
- Refresh-token rotation on the response is handled leniently: if the refresh response omits
  `refresh_token`, the existing one is reused rather than the request being rejected (the Claude
  store's policy). This is a deliberate choice, not a measured fact — see §11.

## 6. Request shaping (`Credential::KimiOauth`, `src/adapters/anthropic/mod.rs`)

For an outbound request routed through a `kimi_oauth` provider, shunt:

- Strips any inbound `authorization`/`x-api-key` headers and sets `authorization: Bearer
  <access_token>`.
- Adds the five `X-Msh-*` identity headers Kimi requires on every call to `auth.kimi.com` and
  `api.kimi.com`: `x-msh-platform: shunt`, `x-msh-version: <shunt's own crate version>`,
  `x-msh-device-name` (the machine's hostname, `"unknown"` if unavailable),
  `x-msh-device-model` (`"<os>-<arch>"`), and `x-msh-device-id` (the account's persisted device
  id from §3).
- Does **not** add an `anthropic-beta: oauth-2025-04-20` header the way `claude_oauth` does —
  that header, and whether `api.kimi.com` accepts or rejects it if sent, is specific to
  Anthropic's own OAuth path and has not been carried over or tested for Kimi (see §11).
- A `token_env`-sourced Kimi credential (no account file, so no persisted device id) falls back to
  one UUID v4 generated once per shunt process and reused for every request that credential makes.
  A per-request id would present a single account to Kimi as a different device on every call,
  which is the churn pattern a device-bound API is most likely to penalize.

## 7. Config & validation (`AuthMode::KimiOauth`, `src/config.rs`)

A `kimi_oauth` provider is accepted only if all of the following hold (mirroring the
`claude_oauth`/`chatgpt_oauth`/`xai_oauth`/`cursor_oauth`/`google_oauth` pattern already in
`src/config.rs`):

- `kind = "anthropic"` — Kimi Code's coding API speaks the Anthropic Messages wire shape, and the
  anthropic adapter is the only one that injects the Kimi bearer and `X-Msh-*` headers.
  (`ConfigError::KimiOauthWrongKind` otherwise.) This pin is load-bearing: relaxing it would let
  another adapter forward the client's own credential off-origin instead.
- `base_url` scheme is `https` (`ConfigError::KimiOauthNotHttps`), unless the host is a loopback
  address (local dev/testing exception, same as the other OAuth modes).
- `base_url` host is `kimi.com` or any subdomain — which covers the measured `api.kimi.com` API
  host (`ConfigError::KimiOauthNonKimiHost` otherwise). This guard exists so shunt never sends a
  Kimi Code subscription bearer to a non-Kimi origin (e.g. a config typo, or a gateway/proxy
  base_url that shouldn't see it).
- The legacy `[providers.<name>]` table form's `accounts` array is valid with `auth =
  "claude_oauth"`, `"chatgpt_oauth"`, or `"kimi_oauth"` (`ConfigError::AccountsRequireOauthProvider`
  otherwise) — `kimi_oauth` was added to an existing three-way check, not a new one.

## 8. Pool integration

`kimi_oauth` reuses the same `StoreFamily`/`resolve_pool_accounts` machinery as
`claude_oauth`/`chatgpt_oauth` (see [`m8-anthropic-multi-account.md`](m8-anthropic-multi-account.md)
and [`m10-codex-multi-account.md`](m10-codex-multi-account.md)):

- `providers.<name>.auth = { mode = "kimi_oauth", account = "name" }` or `accounts = [...]`
  (not both) selects a single account or a scoped pool from the Kimi store; omitting both scans
  every account in `~/.shunt/accounts/kimi/`.
- The request-forwarding path (`forward_kimi_oauth`, `src/adapters/anthropic/mod.rs`) rotates
  across the resolved accounts on failure exactly like the Claude/Codex pools, cooling down an
  account that fails over rather than retrying it immediately. There is no same-account forced
  refresh/retry path — `KimiAuthStore` only refreshes lazily via `get_valid`.
- `GET /admin/pool` and `GET /usage` both include Kimi accounts via the same generic
  `AuthMode::ClaudeOauth | AuthMode::ChatgptOauth | AuthMode::KimiOauth` filter, each with a
  dedicated `StoreFamily::Kimi` resolution arm.

## 9. Admin web surface

The admin web surface's pool dashboard lists Kimi accounts (read-only, via the `GET /admin/pool`
integration above), but there is no browser-driven provisioning route for Kimi — unlike Claude
(`/admin/accounts`) and Codex (`/admin/accounts/codex`), no `/admin/accounts/kimi` route (GET,
POST, or DELETE) is registered anywhere in `src/admin/mod.rs`. A Kimi Code account can only be
added or removed with the CLI (`shunt login kimi --name <account-name>`, or by deleting its
account file).

## 10. Model discovery

`kimi_oauth` providers use shunt's builtin model catalog; shunt does not query the Kimi Code
upstream's own model-listing endpoint for them
(`AuthMode::KimiOauth => return None` in `src/discovery/upstream.rs`). This isn't an oversight —
`api.kimi.com/coding` does answer `GET /v1/models` (an unauthenticated probe returns 401 rather
than 404), but that 401 body carries an **OpenAI-shaped** error envelope, not the Anthropic one
its `/v1/messages` endpoint returns. Since the *authenticated* response's shape has not actually
been measured, discovery falls back to the builtin catalog rather than assuming it matches the
Anthropic list-models envelope the discovery code otherwise parses. Operators should route to
whichever model ids their own subscription exposes.

## 11. Security

- Never log an access token, refresh token, or the raw `kimiOauth`/`Authorization` header value —
  matching every other OAuth store in this codebase.
- Account files are `0600`, their directory `0700`, written atomically (temp file + rename) so a
  crash mid-write cannot leave a torn or partially-written credential on disk.
- The host/scheme/kind validation in §7 exists specifically to prevent a config mistake from
  sending a live Kimi Code subscription bearer to a non-Kimi, non-loopback, or non-`https` origin.
- Read `src/auth/kimi/auth.rs`'s and `src/auth/kimi/login.rs`'s module doc comments before
  changing the poll/refresh parsing order — the "parse the body before checking status" rule is
  load-bearing for correctness (§2), not a style preference.

## 12. Open questions

- **Live-API validation.** Not yet done. Everything in this document is grounded in shunt's own
  code and its test suite's mocked fixtures (each annotated in-source as measured against the real
  `auth.kimi.com` endpoints where noted), not an actual end-to-end login and request against a live
  Kimi Code subscription. A live-verification pass is tracked separately and remains open.
- **Refresh-token rotation.** Unmeasured whether Kimi always rotates the refresh token on a
  refresh grant, sometimes does, or never does. shunt treats a response that omits
  `refresh_token` leniently (reuses the existing one, §5) rather than rejecting it — the opposite
  of xAI's always-rotates assumption. If live testing shows Kimi always rotates and expects the
  old token invalidated, this policy should be revisited.
- **`anthropic-beta` header.** shunt does not send one on the Kimi path (§6). Whether
  `api.kimi.com/coding` accepts, ignores, or rejects Anthropic-specific beta headers at all is
  unmeasured.
- **Rate limits, quota windows, and latency.** No live request has been made against
  `api.kimi.com/coding` through this path, so there is no data yet on response headers, quota
  windows, or typical latency to compare against Claude's or Codex's pools.
- **Model catalog accuracy.** The builtin catalog entries under the `kimi-code` provider are
  best-effort and not verified against what a real subscription actually exposes (§10);
  operators should treat the ids their own subscription lists as authoritative over the catalog.
- **Tier/entitlement gating.** Unknown whether every Kimi Code subscription tier can use this
  OAuth path, or whether (like xAI's SuperGrok Heavy gate) some tiers are restricted. No gating
  behavior has been observed because no live login has been attempted.
