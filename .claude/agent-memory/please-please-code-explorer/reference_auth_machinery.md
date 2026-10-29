---
name: reference-auth-machinery
description: Map of shunt's reusable OAuth/credential-store helpers (src/auth/shared*, callback.rs, codex/xai/kimi stores, inbound_jwt JWKS) for building a new per-account OAuth provider
metadata:
  type: reference
---

Reusable pieces (verify line numbers before citing; they drift):
- PKCE + state: `auth::shared::generate_pkce` (shared.rs ~39). Redirect-hardened POST client: `token_refresh_client` (~205). Token URL env override guard: `sanitize_token_url` / `admin_token_url_override` (~155/~173). Endpoint safety predicate: `is_safe_refresh_url` (~124).
- Born-private writes: `shared::write_account_file` (dir 0700, file 0600, atomic via `src/atomic_file.rs`); `write_account_file_locked` adds flock. Name rule: `validate_account_name`. Dir: `default_accounts_dir(env, subdir)` => `~/.shunt/accounts/<subdir>/<name>.json`.
- Refresh single-flight per canonical path: `auth/shared/refresh_lock.rs` (`refresh_lock_for`, `normalized_auth_path`); detached `tokio::spawn` task owns guard through writeback (codex/auth.rs `refresh_and_write_back`, xai/auth.rs `get_valid`). Cross-process flock: `auth/shared/file_lock.rs` (`FileLockKind`, `lock_file`), used only by gateway session + antigravity.
- Loopback callback: `auth/callback.rs` `CallbackServer::bind(CallbackConfig, state)`; claude/login.rs has auto + `--manual` paste fallback (state passed as `code#state`).
- JWT/JWKS: `jsonwebtoken` v11 (aws_lc_rs) in `auth/inbound_jwt.rs` + `inbound_jwt/jwks.rs`; `JwksCache::key_for(&JwtIssuerRule, kid)`; no nonce check; `validate_claims` is private.
- No token-revocation code exists anywhere; `gateway logout` only unlinks session file under flock.
- Pool wiring: `StoreFamily` enum (accounts.rs:29), `ProviderConfig::store_family`/`pool_scan` (config.rs ~3290), `AuthMode` (config.rs:2317), `config/upstreams.rs` AuthMap, `resolve_chatgpt_account` (auth/mod.rs:464), pool failover in adapters/responses/pool.rs `resolve_or_cooldown`.
- Closest template for new refreshable per-account provider: codex (typed terminal errors) + xai (timeouts, forced refresh mark). Kimi uses a single global refresh lock (not a good model).
