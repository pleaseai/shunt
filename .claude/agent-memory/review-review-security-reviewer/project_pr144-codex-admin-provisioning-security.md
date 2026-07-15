---
name: pr144-codex-admin-provisioning-security
description: PR #144 admin-web Codex(ChatGPT) OAuth provisioning security posture — verified-safe list + the one redirect-hardening residual on the code-exchange path.
metadata:
  type: project
---

PR #144 (`amondnet/codex-account-web`) adds ChatGPT OAuth account provisioning to
the opt-in `[server.admin]` web surface (`src/admin/codex.rs`, `src/auth/codex/{login,store,auth}.rs`,
`src/auth/shared.rs`), mirroring the already-reviewed Claude admin flow ([[m9-admin-surface-security]],
[[project_m9-admin-surface-security]]).

**Why:** M10 codex multi-account ([[m10-codex-multi-account]]) got a browser provisioning UI like M8/M9 did for Claude.

**How to apply (verified-safe — do NOT re-flag on future passes):**
- PKCE + state: `auth::shared::generate_pkce` (32 rand bytes each, S256, independent state). State checked constant-time via `inbound::constant_time_eq`; verifier+state stored together in the per-name `PendingStore` entry (`codex/{name}` key namespace), single-use, 5-attempt cap.
- Path traversal: `validate_account_name` = `[a-z0-9-]+` enforced at every store entry point (store, remove, import). Cannot escape accounts dir.
- Credential files: born-private 0600 (dir 0700) via `write_account_file`→`write_private` create_new+mode, no chmod-after-write window. Tests assert perms.
- No token egress to browser/logs: handlers log `account=%name`, `%error` (generic msgs, no upstream body), `account_id_present=true` (bool). list/complete responses carry only name/expires_at/account_id. Integration tests assert token strings absent from responses.
- CSRF/same-origin: cookie sessions require Sec-Fetch-Site same-origin/none + matching x-csrf-token (constant-time); SameSite=Strict HttpOnly cookie; header-token callers CSRF-exempt (no ambient cookie). Security headers + tight CSP on all admin responses.
- XSS: dashboard renders all dynamic data via `textContent`/`cell()` (never innerHTML). authorize_url `.href` is server-built to fixed `auth.openai.com` host.
- `parse_callback_value` (redirect-URL or `code#state`): no real parsing pitfall — bad/mismatched inputs fail the downstream state check.
- SSRF: `SHUNT_CODEX_TOKEN_URL` → `sanitize_token_url` (https-any-host OR http-loopback only; blocks plaintext off-host). Refresh POST uses redirect-hardened `token_refresh_client()`. https-to-any-host is a documented, operator-env-gated, accepted residual (same as [[project_claude-token-url-egress]]) — by-design, do not flag.

**The one residual (minor, low confidence ~40):** the OAuth *code-for-token* exchange
(`codex/login.rs::exchange_code`) uses the injected `state.http_client`
(`reqwest::Client::new()` at main.rs:254 → follows up to 10 redirects), NOT the
redirect-hardened `token_refresh_client()` the refresh path uses. A 3xx from the token
endpoint to a plaintext/off-host target would resend the single-use auth `code` + PKCE
`code_verifier` together, defeating PKCE. Precondition = malicious/misconfigured token
endpoint (default is auth.openai.com HTTPS), so real-world exploitability is low, and the
Claude admin flow has the identical pattern (`complete_account` also uses `state.http_client`).
Defense-in-depth nicety, not a live vuln.
