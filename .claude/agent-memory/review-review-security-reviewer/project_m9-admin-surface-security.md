---
name: m9-admin-surface-security
description: Security posture of the M9 admin web surface (src/admin/) — what is hardened and the residual defense-in-depth gaps found in review.
metadata:
  type: project
---

The opt-in admin web surface (`src/admin/`, issue #77, branch amondnet/77) provisions upstream Claude accounts from a browser. Reviewed 2026-07-13.

**Well-hardened (do not re-flag):**
- CSRF is triple-layered on cookie-auth mutations: `SameSite=Strict` cookie (Path=/admin) + per-session synchronizer token (`x-csrf-token`, constant-time compared) + `Sec-Fetch-Site`/Origin same-origin check. Header-token (curl) path is correctly CSRF-exempt (no ambient cookie; custom header needs a CORS preflight the server never grants).
- Path traversal: `validate_account_name` requires `[a-z0-9-]+`, re-checked inside `store_setup_token`/`remove_account`. Solid on DELETE and store writes.
- Secrets: the setup token and PKCE verifier are never returned to the browser or logged (tests assert). The authorize_url returned to the browser carries the S256 challenge and the OAuth `state` (intentionally — `state` is a CSRF nonce, not a bearer secret), but never the verifier.
- XSS: HTML pages render dynamic data via `textContent`; only the random-base64 csrf + fixed login error go through `escape_html`.
- Admin token compare is constant-time (`InboundAuth::authenticate_value`). Fail-closed at boot (`AdminConfig::resolve`) and on reload-disable (`admin_auth: None` → all routes reject).

**Residual gaps (all minor / defense-in-depth):**
- **No rate-limit or lockout on `POST /admin/login`** even though `/complete` got a global rate limiter. `parse_tokens` enforces NO entropy floor (accepts any non-empty token) → a weak admin token is brute-forceable unthrottled. See [[Why:]] the guide (`admin-remote-provisioning.md`) is explicitly about exposing this over SSH/tunnel.
- **`secure_cookie` derives the `Secure` flag from the client-controlled `Host` header** (`host_is_loopback`). A reverse proxy that upstreams `Host: localhost` (nginx `proxy_pass http://localhost` default) drops `Secure` on a public HTTPS deploy. Documented carve-out (mirrors M8).
- **No security headers** (CSP / X-Frame-Options / X-Content-Type-Options) on the admin HTML/JSON responses (`html_body` sets only Content-Type). Clickjacking is largely neutralized by SameSite=Strict.
- **`SHUNT_CLAUDE_TOKEN_URL`** (`admin_token_url`) has no host allowlist; `/complete` posts the OAuth code+verifier there. Env-gated / operator-controlled, not attacker-reachable. Overlaps [[claude-token-url-egress]].

**Update 2026-09-11 — SPA cutover (PR #526, branch amondnet/admin-spa-cutover):** the server-rendered dashboard (`html::dashboard_page`, `admin::script`) is deleted; `GET /admin` now serves the static embedded shell **unauthenticated** under `--features ui` (`ui::shell`), `404` naming the feature without it. Verified safe and *not* to be re-flagged:
- `ui/dist/index.html` is a generic shell (title + `<div id=root>` + two hashed asset links) — no operator data, no per-session value. The admin router is still mounted only when `[server.admin]` is set, so the existence disclosure is unchanged from the old `303 -> /admin/login`.
- The security-headers gap above is **closed**: `ui::SHELL_CSP` is strictly tighter than the login page's (`script-src 'self'`, `style-src 'self'`, `form-action 'none'`, `base-uri/frame-ancestors 'none'`) plus DENY/nosniff/no-store/Referrer-Policy. No directive was weakened anywhere.
- The CSRF token now comes from `GET /admin/api/session` (401 when unauthenticated). The "no CORS layer" claim is true — `grep -riE "cors|access-control-allow" src/` hits only that comment. `check_csrf` + `require_write` still guard all 7 mutations (4 in `mod.rs`, 3 in `codex.rs`).
- Login page keeps `script-src 'unsafe-inline'`/`connect-src 'self'` (tracked as issue #525): no script, no fetch, and both interpolated values go through `escape_html` (&<>"'), so there is no reachable injection — do not report.
