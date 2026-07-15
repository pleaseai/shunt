# M2 — ChatGPT / Codex authentication (spec)

> Companion to [`implementation-plan.md`](implementation-plan.md) §4 and
> [`m1-responses-translation.md`](m1-responses-translation.md). Covers the `auth/` module
> (`auth/mod.rs`, `auth/codex_auth.rs`, `auth/login.rs`). M2 reuses the whole M1 translation
> core unchanged; it only adds credential acquisition for the `codex`/`chatgpt` provider (and
> resolves the `openai` API-key path). Reference: `insightflo/chatgpt-codex-proxy/src/auth.ts`
> and the real `~/.codex/auth.json` written by `codex login`.

## 0. Model slugs — verified against the live backend (2026-07-09)

The ChatGPT-account Codex backend (`/backend-api/codex/responses`) **rejects the
`gpt-*-codex` slugs** the insightflo reference hardcodes (e.g. `gpt-5.2-codex`,
`gpt-5.1-codex`) with `400 {"detail":"The 'X' model is not supported when using Codex with a
ChatGPT account."}`. It only accepts the **account's live-entitled slugs**, which the codex CLI
fetches from its `/models` endpoint and which vary by plan. Verified end-to-end: a **free**
ChatGPT account resolves to **`gpt-5.5`** (the codex CLI itself used `gpt-5.5`; `gpt-5.6-sol`
and `gpt-5.2` were rejected for that account).

Implications:
- **Do NOT hardcode a `-codex` model map.** shunt passes the request `model` through (or a
  route's `upstream_model`), so the developer picks a currently-entitled slug via
  `ANTHROPIC_CUSTOM_MODEL_OPTION` (e.g. `gpt-5.5`). The stale table in
  [`m3-discovery.md`](m3-discovery.md) §5 / `insightflo`'s `models.ts` is reference-only.
- To discover the exact usable slugs for an account, look at what `codex` itself sends, or its
  bundled catalog `codex-rs/models-manager/models.json` filtered by the account's
  `chatgpt_plan_type` — but the authoritative source is the live `/models` fetch.
- shunt surfaces the backend's `detail` message on error (fixed 2026-07-09), so a wrong slug
  now returns the real reason rather than a generic "upstream request failed".

## 1. Scope

- Acquire a valid **ChatGPT OAuth access token + account id** for the `codex`/`chatgpt`
  provider, refreshing on expiry.
- Resolve the **OpenAI API key** for the `openai` provider.
- Primary credential source is the existing **`~/.codex/auth.json`** (D3 RESOLVED: read +
  auto-refresh + write-back, with a file lock). Fallback: a self-contained `shunt login`.

Out of scope: any change to request/response translation (that is M1).

## 2. Credential sources & precedence

`Credential` enum: `ApiKey(String)` | `ChatGptOAuth { access_token, account_id }`.

**`openai` provider (API key):**
1. `OPENAI_API_KEY` env, else
2. `~/.codex/auth.json` `.OPENAI_API_KEY` when `auth_mode == "ApiKey"` and non-null, else
3. `[providers.openai] api_key_env` indirection in config.
→ header `Authorization: Bearer <key>`.

**`codex`/`chatgpt` provider (ChatGPT OAuth):**
1. `~/.codex/auth.json` `.tokens` (primary), else
2. shunt's own token file `~/.shunt/tokens.json` (written by `shunt login`).
→ headers `Authorization: Bearer <access_token>` + `chatgpt-account-id: <account_id>`
   (plus the M1 constant headers `OpenAI-Beta: responses=experimental`, `originator: codex_cli_rs`).

## 3. `~/.codex/auth.json` schema (confirmed)

```jsonc
{
  "auth_mode": "ChatGPT",          // or "ApiKey"
  "OPENAI_API_KEY": null,          // string when auth_mode == "ApiKey"
  "tokens": {
    "id_token":      "<JWT>",
    "access_token":  "<JWT>",      // bearer sent upstream; carries exp + account claim
    "refresh_token": "<JWT>",
    "account_id":    "<uuid, 36 chars>"   // preferred account-id source
  },
  "last_refresh": "2026-..."        // ISO-8601 string
}
```

Notes:
- **Account id:** prefer `tokens.account_id`. Fallback: decode the `access_token` JWT payload
  and read `["https://api.openai.com/auth"].chatgpt_account_id` (confirmed present).
- **Expiry:** there is **no `expires_at` field**. Read the `exp` claim from the `access_token`
  JWT (standard). Treat as expired within a **5-minute buffer** before `exp`.
- `shunt login`'s own file (`~/.shunt/tokens.json`) may use the same schema for consistency.

## 4. OAuth constants (from the Codex CLI flow)

| Constant | Value |
| :-- | :-- |
| `client_id` | `app_EMoamEEZ73f0CkXaXp7hrann` |
| authorize URL | `https://auth.openai.com/oauth/authorize` |
| token URL | `https://auth.openai.com/oauth/token` |
| redirect URI | `http://localhost:1455/auth/callback` |
| scope | `openid profile email offline_access api.connectors.read api.connectors.invoke` |
| PKCE | S256 (`code_challenge` = base64url(sha256(verifier))) |
| extra authorize params | `response_type=code`, `code_challenge_method=S256`, `id_token_add_organizations=true`, `state=<csrf>`, `codex_cli_simplified_flow=true`, `originator=codex_cli_rs` |

> Verified against openai/codex `codex-rs/login/src/server.rs::build_authorize_url`
> (2026-07-15). The admin web flow (`src/auth/codex/login.rs`) mirrors these exactly
> so a web-provisioned account is authorization-equivalent to a `codex login` one.

## 5. `TokenStore` behavior (`auth/codex_auth.rs`)

`async fn get_valid_chatgpt() -> Result<ChatGptCred>`:

1. **Read fresh** from `~/.codex/auth.json` on each call (cheap; the codex CLI may have
   refreshed the file under us — re-reading avoids a redundant refresh and clobber).
2. Parse `tokens.access_token`; decode its JWT `exp`.
3. If `now < exp - 5min`: return `{ access_token, account_id }`.
4. Else **refresh** (see below), persist, return the new pair.
5. If no tokens / refresh fails: error `authentication_error` with a message pointing the
   developer to run `codex login` (or `shunt login`).

**Refresh** (`POST token URL`, `application/x-www-form-urlencoded`):
`grant_type=refresh_token`, `refresh_token=<current>`, `client_id=<client_id>`.
Response → new `access_token`, `refresh_token` (may rotate), `id_token`, `expires_in`.

**Write-back** (D3): acquire a **file lock** (e.g. `fs2`/advisory lock on a `.lock` sibling),
**re-read** the file, update `tokens.{access_token,refresh_token,id_token}`, recompute
`tokens.account_id` from the new access token, set `last_refresh = now (ISO-8601)`, **preserve
all other fields** (`auth_mode`, `OPENAI_API_KEY`), write atomically (temp file + rename),
restore `0600` perms. Never reorder/drop unknown keys — the codex CLI owns this file too.

Concurrency: the lock guards shunt-vs-shunt; the re-read-before-refresh guards shunt-vs-codex.
If a refresh loses the race (file changed to a still-valid token), prefer the file's token.

## 6. `shunt login` fallback (`auth/login.rs`)

Only for environments without `codex login`. Mirrors §4/§5 of the reference:

1. Generate PKCE verifier/challenge + random `state`.
2. Start a loopback `axum` server on `127.0.0.1:1455`, route `/auth/callback`.
3. Build the authorize URL (§4 params) and open the browser (`open`/`xdg-open`/`start`);
   also print it for headless copy-paste.
4. On callback: verify `state` (CSRF), exchange `code` + `verifier` at the token URL
   (`grant_type=authorization_code`, `client_id`, `code`, `code_verifier`, `redirect_uri`).
5. Persist to `~/.shunt/tokens.json` (schema §3), `0600`.
6. **5-minute timeout**; handle `EADDRINUSE` on :1455 (another login in progress) with a clear
   message. Serve a minimal success HTML page.

CLI surface: `shunt login`, `shunt logout` (delete token file), `shunt auth status`
(logged-in / expired / has-refresh / expires-at) — mirror the reference's status helper.

## 7. Wiring into the proxy

When routing resolves the provider to `codex`/`chatgpt`, the proxy asks the `TokenStore` for a
valid ChatGPT credential and injects the two headers before sending the (M1-translated)
Responses request to `https://chatgpt.com/backend-api/codex/responses`. A `401` from upstream
maps to `authentication_error` (M1 §8) and should hint at re-auth; **do not auto-retry a bot
loop** — surface it.

## 8. Rust crates & modules

| Concern | Crate |
| :-- | :-- |
| OAuth PKCE + token exchange/refresh | `oauth2` (or hand-rolled `reqwest` form POST — small surface) |
| JWT payload decode (no verify) | `base64` + `serde_json` (split on `.`, decode `[1]`) |
| File lock | `fs2` (advisory) |
| Browser open | `open` crate |
| Loopback callback | reuse `axum` (already a dep) |
| Time / ISO-8601 | `time` or `chrono` (already pulling one via deps ideally) |

`auth/mod.rs`: `Credential`, `TokenStore` trait, provider→credential resolution.
`auth/codex_auth.rs`: `~/.codex/auth.json` reader/refresher/writer.
`auth/login.rs`: `shunt login` PKCE loopback flow + `logout`/`status`.

## 9. Security

- **Never log** access/refresh/id tokens or the API key. Log only: `auth_mode`, account-id
  presence, expiry timestamp, refresh success/failure.
- Enforce `0600` on any token file shunt writes; atomic temp-file + rename.
- Redirect URI is loopback only; verify `state` on every callback.
- Treat `~/.codex/auth.json` as sensitive — no copying into logs/telemetry/worktrees. (It is
  **not** in `.worktreeinclude`; only `.env*` and `settings.local.json` are.)

## 10. Tests

- JWT `exp` + account-id claim extraction from a fixture token (fabricated, not a real token).
- auth.json parse for both `auth_mode` values; API-key precedence chain.
- Refresh: `wiremock` the token URL; assert form body + that write-back preserves unknown
  fields and updates `last_refresh`.
- `shunt login`: state-mismatch → rejected; happy path exchanges code and persists (token URL
  mocked).
- Expiry buffer boundary (just-inside / just-outside 5 min).

## 11. Open questions

- **Q1 — write-back vs read-only.** Spec assumes write-back to `~/.codex/auth.json` (D3). If we
  prefer never to touch the codex CLI's file, alternative: refresh in-memory only and write to
  `~/.shunt/tokens.json` instead, reading codex's file read-only. Slightly more re-refreshing,
  zero clobber risk. Decide before M2 impl.
- **Q2 — `originator` / `OpenAI-Beta` values** are pinned to the codex-CLI flow; if OpenAI
  changes them, the codex CLI version is the source of truth — keep them in one constants module.
- **Q3 — API-key path for `openai`** may also want the standard `https://api.openai.com/v1`
  Responses endpoint vs an org gateway; keep `base_url` in config (already in schema).
```
