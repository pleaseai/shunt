# M6 — xAI Grok provider (spec)

> **⚠️ Experimental — not yet verified against the live xAI API.** The flow is validated
> against the reference implementations (Hermes, OpenCode) and unit tests with mocked
> endpoints only; it has not been exercised with a real SuperGrok account or `XAI_API_KEY`.
> Endpoints, request quirks, and error semantics may change once verified live.

> Companion to [`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md) and
> [`m1-responses-translation.md`](m1-responses-translation.md). Adds the `xai` provider: the
> xAI Grok Responses API, reachable two ways — an **API key** (`XAI_API_KEY`) or a
> **subscription OAuth** login (SuperGrok / X Premium+) via the RFC 8628 device-code flow.
> Reuses the whole M1 translation core; only the per-backend quirks and a new credential
> source are added. Reference implementations: OpenCode's `xai.ts` and Hermes' `auth.py` /
> `transports/codex.py` (device flow, refresh, request shaping).

## 1. Scope

- A built-in `xai` provider (`kind = "responses"`, `base_url = https://api.x.ai/v1`) that
  defaults to the **API-key** path (`XAI_API_KEY`) and can be flipped to **subscription OAuth**
  (`auth = "xai_oauth"`) in `shunt.toml`.
- A `shunt login xai` subcommand that runs the device-code flow and writes a shunt-owned
  credential file, refreshed automatically on expiry.
- xAI-flavored request translation (a third [`ResponsesFlavor`] beside OpenAI and ChatGPT/Codex).

Out of scope: any change to the M1 response translation / SSE state machine.

## 2. OAuth constants (verified against Hermes + OpenCode)

| Constant | Value |
| :-- | :-- |
| issuer | `https://auth.x.ai` |
| device authorization URL | `https://auth.x.ai/oauth2/device/code` |
| token URL | `https://auth.x.ai/oauth2/token` |
| `client_id` | `b1a00492-073a-47ea-816f-4c329264a828` (public Grok-CLI client, no secret) |
| scope | `openid profile email offline_access grok-cli:access api:access` |
| device-code grant | `urn:ietf:params:oauth:grant-type:device_code` |
| API endpoint | `POST https://api.x.ai/v1/responses` (OpenAI Responses shape) |

All token/device requests are `application/x-www-form-urlencoded` with `Accept: application/json`.
The API request carries `Authorization: Bearer <access_token>` only — no account-id or
`OpenAI-Beta` header.

## 3. Credential file (`~/.shunt/xai-auth.json`)

Shunt-owned (unlike the codex path, nothing else writes it). Override with `SHUNT_XAI_AUTH_FILE`.
Written atomically (temp file + rename) at `0600`.

```jsonc
{
  "tokens": {
    "access_token":  "<JWT>",   // bearer sent upstream; carries exp
    "refresh_token": "<opaque>", // ROTATED on every refresh — must be persisted
    "id_token":      "<JWT>"     // optional
  },
  "last_refresh": "2026-..."      // ISO-8601
}
```

- **Expiry:** no `expires_at` field. Read the `exp` claim from the `access_token` JWT
  (unverified decode, like codex_auth) and treat as expired within a **5-minute buffer**.
  Device-code tokens can be short-lived (~15 min), so refresh is frequent.
- **Refresh-token rotation:** every refresh consumes the old refresh token and returns a new
  one. shunt persists the rotated pair or the next refresh fails. A refresh success that
  omits `refresh_token` is treated as an invalid response (nothing persisted) rather than
  leaving the consumed token on disk.

## 4. Device-code flow (`shunt login xai`, `auth/xai_login.rs`)

1. `POST device/code` with `client_id` + `scope`. Response: `device_code`, `user_code`,
   `verification_uri`, `verification_uri_complete`, `expires_in`, `interval`.
2. Print `verification_uri_complete` (fallback `verification_uri` + `user_code`) to stdout so
   the operator opens it in a browser on any device.
3. Long-poll `POST token` with `grant_type=device_code`, `client_id`, `device_code`:
   - interval floored to ≥1s; `authorization_pending` → keep polling; `slow_down` → interval
     `+5s` capped at 30s; `access_denied` / `authorization_denied` / `expired_token` → terminal;
     loop until the `expires_in` deadline (then time out).
   - success must contain **both** `access_token` and `refresh_token`.
4. Persist to `~/.shunt/xai-auth.json` (§3) and print success + the token expiry.

No loopback callback server is needed — the polling loop is the only surface, so this works on
VPS / SSH / Docker / CI where an inbound `127.0.0.1` port isn't reachable from the browser.

## 5. Token store behavior (`auth/xai_auth.rs`)

`get_valid()`:
1. Read the credential file fresh; decode the access-token `exp`.
2. If `now < exp − 5min`: return the access token.
3. Else take the process-wide **refresh lock** (`tokio::sync::Mutex` single-flight — xAI
   rotates the refresh token, so a losing concurrent refresh would replay a consumed token),
   then **re-read** and re-check: a waiter finds the winner's rotated pair and returns it.
4. Else **refresh** under the lock (`grant_type=refresh_token`, `client_id`,
   `refresh_token`), write the rotated pair back atomically, return the new access token.
   Cross-process races are out of scope — shunt owns the file and one gateway process is
   the norm.

**Refresh error mapping** (distinct, per Hermes #26847):
- **403** → the OAuth grant is valid but the account is **not entitled to API access**
  (subscription-tier gate). Re-login won't fix it — the error points at the `XAI_API_KEY`
  path / an upgrade, and does **not** tell the user to log in again.
- **400 / 401** → `invalid_grant` (consumed/invalid refresh token) → tells the user to
  run `shunt login xai`.
- other non-2xx → generic refresh-failure message.

All gateway-owned errors use the Anthropic error envelope via the `auth_error` helper.

## 6. Request shaping — the `xai` [`ResponsesFlavor`]

Detected table-driven, not by provider name: `auth = chatgpt_oauth` → ChatGPT; a base_url host
of `x.ai`/`*.x.ai` → xAI (covers both the API-key and OAuth `xai` providers); else OpenAI. The
xai flavor differs from the stock OpenAI translation in three ways (learned from 400s in the
reference impls):

| Field | OpenAI | ChatGPT/Codex | **xAI** |
| :-- | :-- | :-- | :-- |
| `store` | `false` | `false` | `false` |
| `service_tier` | never sent | never sent | never sent (xAI 400s on it) |
| `reasoning` | always `{effort, summary:auto}` | always `{effort, summary:auto}` | **only when effort explicitly chosen** (route/provider config or per-request `output_config.effort`), and `{effort}` **without** `summary` |
| `text.verbosity` | sent | sent | **omitted** (xAI rejects the `text` object) |
| `max_output_tokens` | sent | dropped | sent |
| `include: [reasoning.encrypted_content]` | when thinking enabled | when thinking enabled | when thinking enabled |

The reasoning gate is the key quirk: several grok models (`grok-4*`, `grok-3`, `grok-code-fast`,
`grok-4.20-0309-*`) **400 on `reasoning.effort`** even though they reason natively. Rather than a
hardcoded model list (AGENTS.md forbids it), shunt keeps the dial **opt-in**: send `reasoning`
only when an effort was explicitly chosen — configured for the route or provider in `shunt.toml`,
or sent per-request via `output_config.effort`. Derived defaults (thinking flag, model suffix)
stay off. Encrypted-reasoning
replay (`include`) stays gated on the client's extended-thinking flag, exactly like the codex path.

## 7. Config & validation (`config.rs`)

- New `AuthMode::XaiOauth`. Built-in `xai` provider seeded in `Config::default()`
  (`kind = responses`, `base_url = https://api.x.ai/v1`, `auth = api_key`,
  `api_key_env = XAI_API_KEY`).
- **Bearer-leak guard:** a provider with `auth = "xai_oauth"` must be `kind = "responses"`
  (the anthropic adapter has no XaiOauth injection and would forward the client's own
  credential), use an **https** base_url (never plaintext), and have a base_url host of
  `x.ai` or `*.x.ai` — else startup fails with `XaiOauthWrongKind` / `XaiOauthNotHttps` /
  `XaiOauthNonXaiHost`. shunt refuses to inject a subscription token toward another origin
  (mirrors Hermes' endpoint re-validation).

## 8. Security

- **Never log** access/refresh/id tokens. Log only refresh outcomes and expiry.
- `0600` on the credential file; atomic temp-file + rename; parent dir created on write.
- The device-code poll loop is the only network surface; the refresh path re-reads before
  writing to avoid clobbering a concurrent refresh.

## 9. Models (reference only — not hardcoded)

`grok-build-0.1` (flagship coding), `grok-4.5`, `grok-4.3`,
`grok-4.20-0309-reasoning` / `-non-reasoning`, `grok-4.20-multi-agent-0309`. Pick a slug via a
`[[routes]]` entry or `ANTHROPIC_CUSTOM_MODEL_OPTION`; shunt passes it through.

## 10. Open questions

- **`text.verbosity`.** Dropped for xai because Hermes never sends it and xAI is reported to
  reject the `text` object. If a future grok build accepts it, this is a safe place to re-enable.
- **Refresh skew.** shunt uses the shared 5-minute buffer. Hermes uses an adaptive skew (up to
  1h for long-lived SuperGrok tokens, tightened for ~15-min device-code JWTs) to avoid burning
  single-use refresh tokens on every call. If device-code tokens prove very short-lived in
  practice, revisit the buffer.
- **Live-API validation.** The flow is verified against the reference implementations and unit
  tests (mocked token endpoint); it has not been exercised against a live SuperGrok account.
