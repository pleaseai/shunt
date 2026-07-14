---
name: codex-multi-account-security
description: Security posture of the Codex/ChatGPT multi-account pool (PR #114) — the one config-validation gap plus the verified-safe bearer/WS-isolation invariants.
metadata:
  type: project
---

Codex/ChatGPT multi-account pooling (PR #114, `amondnet/codex-multi`). Reviewed
`src/config.rs`, `src/auth/codex/{store,auth,login}.rs`, `src/auth/mod.rs`,
`src/adapters/responses/mod.rs`, `src/accounts.rs`, `src/auth/shared.rs`.

**Gap found AND CLOSED in this PR — `chatgpt_oauth` kind guard** (`src/config.rs`
validate loop, now L1006-1010). Originally `chatgpt_oauth` only host/scheme-guarded
with no kind check, unlike `claude_oauth`/`xai_oauth`/`cursor_oauth` (each rejects a
wrong `ProviderKind` via ClaudeOauthWrongKind / XaiOauthWrongKind /
CursorOauthWrongKind). So `kind="anthropic", auth="chatgpt_oauth",
base_url="https://chatgpt.com"` would pass validation, dispatch to the anthropic
adapter, whose `outbound_headers` has `_ => {}` for `Credential::ChatGptOAuth` → the
client's own auth headers pass through to chatgpt.com unchanged (subscription bearer
dropped) — the client-credential off-origin leak the XaiOauthWrongKind doc-comment
says its guard prevents. Operator-misconfig-gated, not remote. **Resolved during
review**: added `ConfigError::ChatgptOauthWrongKind` requiring `kind == Responses`
(test `chatgpt_oauth_requires_responses_kind`, config.rs ~L1496), so the combo above
is now rejected at startup.

**Why:** the sibling WrongKind guards are framed as token-leak prevention in
their own comments; chatgpt_oauth was the only OAuth mode missing it.
**How to apply:** the guard now exists — re-verify it survives any config.rs
auth-validation refactor (regression would re-open the off-origin leak).

**Verified SAFE (do not re-flag):**
- Bearer-leak guard for chatgpt_oauth is otherwise sound: `host_is_chatgpt`
  (`==chatgpt.com || ends_with(".chatgpt.com")`) + https, loopback carve-out
  only — mirrors `host_is_anthropic`/claude_oauth. Subscription bearer + account_id
  only injected in `request_builder`/`websocket_headers` targeting `responses_url`
  (base_url host-guarded). No off-origin egress.
- WS pool isolation holds: `account_pool_key = "{name}::{pool_key}"`; account
  names are `[a-z0-9-]+` (no `:`) so no cross-account key collision. `pool_key`'s
  client component comes from `x-shunt-inbound-client`, which `proxy.rs:218`
  strips from inbound then re-inserts (L237) only from the authenticated client —
  clients cannot spoof it → no cross-client WS reuse.
- No token ever logged (tracing calls carry account.name / error.message /
  status / upstream_error_body only — never access/refresh tokens). Note
  `upstream_error_body` log in `build_upstream_error` is PRE-EXISTING (not in #114
  diff) — see [[project_otel-pii-egress]] / [[project_sentry-pii-egress]].
- Path traversal: account name validated `[a-z0-9-]+` at config-load, in
  `scan_accounts` (disk stems), and in store `import_auth`/`remove_account`
  before reaching `account_path`. No escape.
- Credential files born-private: `write_auth_file_atomic` (0600, create_new,
  fsync, rename) + dir 0700 born-private. Codex store has no chmod-after-write
  window (unlike claude — see [[project_token-file-writers]]).
- `x-shunt-account` response header exposes the operator-chosen pool account
  name to clients (also done by the anthropic adapter). Non-secret metadata,
  minor info-disclosure on a shared gateway; likely intentional.
