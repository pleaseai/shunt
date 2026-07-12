# M6 — xAI Grok provider (spec)

> **⚠️ Experimental.** Live-tested against a real SuperGrok OAuth login (2026-07): the device
> flow, token store, refresh, and bearer injection all work end-to-end. The key correction from
> that test is baked in below — the subscription OAuth path targets the **Grok CLI chat proxy**
> (`cli-chat-proxy.grok.com`), not the developer API (`api.x.ai`), which rejects a subscription
> bearer with a **402** (`personal-team-blocked:spending-limit`) / **403** tier gate. Whether a
> given account can use the OAuth path at all depends on its tier (xAI has gated it to SuperGrok
> Heavy at times); accounts without API entitlement should use the `XAI_API_KEY` path.

> Companion to [`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md) and
> [`m1-responses-translation.md`](m1-responses-translation.md). Adds two built-in providers for
> xAI Grok — an **API key** `xai` (developer API, `XAI_API_KEY`) and a **subscription OAuth**
> `grok` (SuperGrok / X Premium+, RFC 8628 device-code flow). Reuses the whole M1 translation
> core; only the per-backend quirks and a new credential source are added. Reference
> implementations: OpenCode's `xai.ts`, Hermes' `auth.py`, and raine/claude-code-proxy's
> `src/providers/grok` (which pinned down the CLI-proxy endpoint + identity headers).

## 1. Scope

- Two built-in providers (`kind = "responses"`):
  - `xai` — the **API-key** path, `base_url = https://api.x.ai/v1`, `auth = api_key`
    (`XAI_API_KEY`). The developer API, billed per token.
  - `grok` — the **subscription OAuth** path, `base_url = https://cli-chat-proxy.grok.com/v1`,
    `auth = xai_oauth`. The Grok CLI chat proxy, which honors a SuperGrok / X Premium+ login and
    is not subject to API billing. Sends the Grok-CLI identity headers (§6).
- A `shunt login xai` subcommand that runs the device-code flow and writes a shunt-owned
  credential file, refreshed automatically on expiry.
- xAI-flavored request translation, with a distinct Grok CLI flavor for capabilities such as
  hosted web search that are verified only on the subscription proxy.

The Grok CLI proxy supports the Responses hosted `web_search` tool end-to-end. shunt forwards it
only for the `grok` subscription provider and translates the resulting search call, results, and URL
citations back into Anthropic `server_tool_use`, `web_search_tool_result`, and citation blocks. The
same tool remains disabled for the API-key `xai` provider because `api.x.ai` support has not been
verified. **Hosted search is billed separately by xAI (approximately $5 per 1,000 calls), even when
the request uses a Grok subscription.**

Out of scope: any change to the M1 response translation / SSE state machine.

## 2. OAuth constants (verified against Hermes + OpenCode)

| Constant | Value |
| :-- | :-- |
| issuer | `https://auth.x.ai` |
| device authorization URL | `https://auth.x.ai/oauth2/device/code` |
| token URL | `https://auth.x.ai/oauth2/token` |
| `client_id` | `b1a00492-073a-47ea-816f-4c329264a828` (public Grok-CLI client, no secret) |
| scope | `openid profile email offline_access grok-cli:access api:access conversations:read conversations:write` |
| device-code grant | `urn:ietf:params:oauth:grant-type:device_code` |
| API endpoint (OAuth `grok`) | `POST https://cli-chat-proxy.grok.com/v1/responses` (Grok CLI proxy) |
| API endpoint (API-key `xai`) | `POST https://api.x.ai/v1/responses` (developer API) |

The `conversations:read`/`conversations:write` scopes are required by the CLI proxy; the OpenAI
Responses shape is identical on both surfaces. The subscription request also carries the Grok-CLI
identity headers (§6).

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

Detected table-driven, not by provider name: `auth = chatgpt_oauth` → ChatGPT; `auth = xai_oauth`
→ Grok CLI (the OAuth `grok` provider, whose `grok.com` host isn't an `x.ai` host); a base_url host
of `x.ai`/`*.x.ai` → xAI (the API-key `xai` provider); else OpenAI. The Grok CLI flavor inherits
the xAI request-shaping rules below and additionally enables hosted `web_search`, which is verified
only on the subscription proxy.

**Grok-CLI identity headers.** The subscription OAuth path (`Credential::XaiOauth`) additionally
sends `x-xai-token-auth: xai-grok-cli`, `x-grok-client-identifier: grok-shell`,
`x-grok-client-version: 0.2.93`, and `accept: text/event-stream` — the CLI proxy
(`cli-chat-proxy.grok.com`) gates on them and otherwise answers as if the caller were an
unentitled API client. The API-key `xai` path sends the bearer only. Neither sends `OpenAI-Beta`.


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

Live note (2026-07): `grok-4.5` on the CLI proxy **accepts** `reasoning.effort` (HTTP 200 with a
configured `effort = "high"` route), so it is not among the models that 400; the opt-in default
stays as the safe choice for the families that still reject it.

## 7. Config & validation (`config.rs`)

- `AuthMode::XaiOauth`. Two built-in providers seeded in `Config::default()`: `xai`
  (`base_url = https://api.x.ai/v1`, `auth = api_key`, `api_key_env = XAI_API_KEY`) and `grok`
  (`base_url = https://cli-chat-proxy.grok.com/v1`, `auth = xai_oauth`). Both `kind = responses`.
- **Bearer-leak guard:** a provider with `auth = "xai_oauth"` must be `kind = "responses"`
  (the anthropic adapter has no XaiOauth injection and would forward the client's own
  credential), use an **https** base_url (never plaintext), and have a base_url host of
  `x.ai`/`*.x.ai` **or** `grok.com`/`*.grok.com` (`host_is_grok_subscription`) — else startup
  fails with `XaiOauthWrongKind` / `XaiOauthNotHttps` / `XaiOauthNonXaiHost`. shunt refuses to
  inject a subscription token toward any other origin.

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

- **Developer API hosted tools.** The Grok CLI proxy is verified to accept hosted `web_search`;
  `api.x.ai` is not, so the API-key `xai` flavor continues to drop it pending a live probe with an
  `XAI_API_KEY`. Related hosted tools such as `x_search` remain out of scope.
- **`text.verbosity`.** Dropped for xai because Hermes never sends it and xAI is reported to
  reject the `text` object. If a future grok build accepts it, this is a safe place to re-enable.
- **Refresh skew.** shunt uses the shared 5-minute buffer. Hermes uses an adaptive skew (up to
  1h for long-lived SuperGrok tokens, tightened for ~15-min device-code JWTs) to avoid burning
  single-use refresh tokens on every call. If device-code tokens prove very short-lived in
  practice, revisit the buffer.
- **Live-API validation.** ✅ Confirmed end-to-end against a live SuperGrok account (2026-07):
  `grok-4.5` returns HTTP 200 through the `grok` provider (`cli-chat-proxy.grok.com`) for
  non-streaming, streaming, and `reasoning.effort` requests. The `api.x.ai` surface returned a 402
  entitlement gate for the same token, which is what motivated retargeting the OAuth path to the CLI
  proxy with the Grok-CLI headers. (Whether every account tier is entitled on the CLI proxy still
  varies — see below.)
- **Tier gate.** xAI has restricted OAuth API access to SuperGrok Heavy at times (Hermes #26847),
  surfacing as a 402/403 even for active standard subscribers. shunt keeps both paths so an
  un-entitled account can fall back to `XAI_API_KEY`. If the CLI proxy proves to honor all tiers,
  this note can be relaxed.
- **`grok_client_version` pinning.** `0.2.93` is hardcoded (mirrors the reference Grok CLI). If the
  proxy starts rejecting stale client versions, make it configurable like the base_url.
