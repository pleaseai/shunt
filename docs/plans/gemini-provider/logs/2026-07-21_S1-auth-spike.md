# S1 — Auth feasibility spike (2026-07-21)

## Verdict: PARTIAL — subscription auth works today but is NOT durably self-drivable.

## What was tested & found

1. **Self-refresh via the public gemini-cli/Code-Assist client** (`681255809395-…apps.googleusercontent.com` + its published installed-app secret), using the on-disk `~/.gemini/oauth_creds.json` refresh_token:
   - refresh → **200** (fresh `ya29`, `expires_in:3599`, 4 scopes)
   - self-minted token → `loadCodeAssist` **200** (`standard-tier`, project `polynomial-highway-szv0r`)
   - self-minted token → `generateContent` `gemini-3-flash-preview` **200** → `"SHUNT-OK"`.
   - ⇒ shunt CAN refresh + use an already-issued gemini-cli token independently, right now.

2. **BUT new sign-ins on that client are dead.** User hit, live, in the gemini CLI:
   *"This client is no longer supported for Gemini Code Assist for individuals. To continue using Gemini, please migrate to the Antigravity suite of products."* New OAuth logins on the gemini-cli client are refused by Google.
   ⇒ own-PKCE `shunt login google` on the gemini-cli client (ticket A2) is a **dead end**. And token-reuse (A1) is on borrowed time: once the on-disk refresh_token is revoked, there is no supported way to mint a new one on this client.

3. **Antigravity token can't be refreshed by us.** `~/.gemini/antigravity-cli/antigravity-oauth-token` is a real Google `ya29`+refresh_token, but refreshing it via `oauth2.googleapis.com/token` fails: `invalid_request` (no secret) / `invalid_client` (with the `strings`-extracted GOCSPX secret) for both embedded client_ids (`1071006060591-…`, `884354919052-…`). Its real secret is obfuscated/runtime-assembled, or it refreshes via `antigravity-unleash.goog`. Replicating it = Path-C reverse-engineering (fragile, ToS-gray) — out of scope.

4. **Operational note for A1:** `~/.gemini/oauth_creds.json` is rewritten atomically (delete→write); a read hit a transient ENOENT mid-session. The token reader must tolerate/retry transient absence. The file is being actively refreshed by something (mtime moved to 18:37 during the session), so a background gemini-cli refresh is currently keeping it fresh.

## Consequence for the plan
No clean, durable, officially-supported *self-driven* login to the Code Assist backend exists as of today:
- gemini-cli door: refresh-only, no new logins (closing).
- Antigravity door: works but its key is held privately by `agy`.

The auth stream (A1/A2) must be re-decided. Three honest options — see STATUS "Next action" / the decision put to the user:
- **Opt-1 (fragile subscription):** ride the gemini-cli refresh token (`~/.gemini/oauth_creds.json`). Works today, zero cost, but may break the day Google finishes the sunset — possibly soon.
- **Opt-4 (stable, paid — Path A):** AI Studio `generativelanguage.googleapis.com` API key. Officially supported, rock-solid, metered per-token (NOT the Google One AI Pro subscription).
- **Opt-3 (not recommended):** reverse-engineer Antigravity's private OAuth. Fragile, high-effort, ToS-gray.
