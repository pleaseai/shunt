---
name: claude-token-url-egress
description: SHUNT_CLAUDE_TOKEN_URL env override sends the refresh_token to any host with no anthropic/https guard, unlike base_url which is validated.
metadata:
  type: project
---

`ClaudeAuthStore::new` (src/auth/claude_auth.rs) reads `SHUNT_CLAUDE_TOKEN_URL` to
override the OAuth refresh endpoint (default `platform.claude.com`). The refresh
POST sends the long-lived `refresh_token` to that URL. Unlike `base_url` (which
`Config::validate` guards with `host_is_anthropic` + https, loopback carve-out),
this override has NO host/scheme validation — any host, plaintext allowed.

Introduced by the M8 multi-account PR (#70): previously TOKEN_URL was a hardcoded
const (unspoofable); now it is env-overridable. Same test-hook pattern as
`SHUNT_CURSOR_BASE_URL`.

**Why:** minor — env-gated, so only an attacker who already controls the shunt
process environment can exploit it (they could already read the credential files).
But it silently defeats the "subscription token never leaks off-origin" invariant
that base_url validation advertises, and the refresh_token is the crown-jewel
long-lived credential.

**How to apply:** if hardening, validate SHUNT_CLAUDE_TOKEN_URL the same way
base_url is (anthropic host + https, loopback carve-out for test mocks). Related:
[[project_token-file-writers]] (the other claude_auth writeback gap).
