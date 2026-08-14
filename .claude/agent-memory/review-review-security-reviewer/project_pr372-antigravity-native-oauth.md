---
name: pr372-antigravity-native-oauth
description: PR #372 native Antigravity HTTP upstream — credential-slot audit verified safe; residual = fixed-port loopback login with no PKCE plus a silent best-effort ::1 bind.
metadata:
  type: project
---

PR #372 (`feat/antigravity-native-upstream`) adds `Credential::AntigravityOauth`
and `AuthMode::AntigravityOauth` on top of merge-base `c00b03b`.

**Verified safe (do not re-litigate):**
- Config validation pins `antigravity_oauth` → `kind = "antigravity"`, https-only,
  host exact-match `cloudcode-pa.googleapis.com` (`host_is_google_codeassist` is an
  `==`, not a suffix match), plus a legacy-kind refusal. `src/config.rs:3107+`.
- All three Responses slots (`request.rs`, `inbound.rs`, `websocket.rs`) name the
  new variant explicitly and send nothing — genuinely fail-closed.
- Token file uses `write_auth_file_atomic` → born-private (0600) writer.
- Hardcoded Google client id/secret at `src/auth/antigravity/auth.rs:42-44` is an
  **intentional, documented** RFC 8252 §8.5 public native-app secret. Never flag its
  presence; only flag it being logged/echoed/written loosely. It is not.
- Manifest-derived `User-Agent` cannot CRLF-inject: `http::HeaderValue` rejects
  `\r\n\0` and control chars, so a poisoned value errors the request instead.

**Residual gaps found (see PR review):**
1. `src/auth/antigravity/login.rs` is the only shunt login **without PKCE** —
   `claude/login.rs` and `codex/login.rs` both send `code_challenge`+S256 via the
   shared `auth::shared::generate_pkce()`. Combined with a **fixed** port 51121 and
   a public client secret, a local process that wins the port yields a usable code.
2. `src/auth/callback.rs:157-165` — on a fixed port the `[::1]` bind is best-effort
   and failure is only `tracing::debug!` (not initialized during `shunt login`).
   The redirect URI advertises `localhost`, which browsers commonly resolve to
   `::1` first → a squatter on `[::1]:51121` receives the authorization code while
   shunt's v4 listener sits idle.

**Why to apply:** any future loopback login that must pin a port inherits this;
PKCE is the control that makes port squatting harmless. Check both when a new
provider login lands.
