---
name: gateway-login-cli-security
description: shunt gateway login/token/logout/claude client CLI — credential-handling posture, what was verified safe, and the 3 residual gaps
metadata:
  type: project
---

`src/auth/gateway/{login,auth,store,launch}.rs` (client side of `[server.gateway]`, RFC 8628 device flow, session cached at `$SHUNT_GATEWAY_SESSION_FILE` else `~/.shunt/gateway/session.json`).

**Verified safe** (do not re-flag without new evidence):
- Session file is born-private: `shared::write_account_file` -> `create_private_dir` (0700) + `atomic_file::write_private` (`create_new` + `mode(0o600)` on the *temp* file too). No chmod-after-write window.
- `GatewaySession`, `TokenResponse`, `DeviceCode` all have hand-written redacting `Debug`; no token reaches logs/errors.
- `shunt gateway token` prints only the token on stdout (integration-tested in `tests/gateway_cli.rs`, including the failure path). Every warning goes to stderr.
- All credential POSTs use `shared::token_refresh_client()` (redirect policy: https-or-loopback only).
- `launch.rs` builds `apiKeyHelper` with proper POSIX single-quote escaping and the `--settings` doc via `serde_json`, not `format!`.
- `logout` deletes the session file (only the empty `.lock` sibling survives).

**Residual gaps found:**
1. `login.rs:97` — server-supplied `verification_uri_complete` goes to `open`/`xdg-open` with no scheme check (file://, smb://, local path, or a leading-dash token).
2. `auth.rs:115-116` — discovery-document `token_endpoint`/`device_authorization_endpoint` are used unvalidated: no https requirement, no same-origin check. The redirect policy guards only redirects, not the initial URL. A test deliberately asserts a cross-host endpoint is accepted.
3. `login.rs:147` — the plaintext-http warning fires only at login; later `gateway token` refreshes over the stored `http://` URL are silent.

**Why:** these are trust-boundary calls, not bugs — the operator types the gateway URL. **How to apply:** if a future PR adds host pinning or a scheme allowlist, gaps 1-3 close together; if it widens what the discovery document can steer, re-review.
