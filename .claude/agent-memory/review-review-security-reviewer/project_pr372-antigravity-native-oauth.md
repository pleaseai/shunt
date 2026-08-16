---
name: pr372-antigravity-native-oauth
description: PR #372 native Antigravity HTTP upstream — credential-slot audit verified safe; the fixed-port loopback login's PKCE and ::1-bind gaps found in review are both closed.
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

**Gaps found in initial review, both since closed — do not re-flag:**
1. ~~`src/auth/antigravity/login.rs` had no PKCE~~ — fixed by `209abf4`:
   `login.rs` now sends `code_challenge`+S256 via the shared
   `auth::shared::generate_pkce()`, same as `claude/login.rs`/`codex/login.rs`.
2. ~~`src/auth/callback.rs`'s fixed-port `[::1]` bind was best-effort and only
   `tracing::debug!` on failure~~ — fixed by `9e76480`: an `AddrInUse` on `[::1]`
   is now refused loudly (named in the returned error, not swallowed as a debug
   log), so a squatter on `[::1]:51121` can no longer silently steal the
   authorization code while shunt's v4 listener sits idle.

**Why to apply:** any future loopback login that must pin a port inherits this
pattern — PKCE is the control that makes port squatting harmless, and a loud
`[::1]` bind failure is what makes squatting detectable in the first place.
Check both when a new provider login lands.
