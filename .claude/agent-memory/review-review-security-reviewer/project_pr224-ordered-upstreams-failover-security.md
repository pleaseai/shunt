---
name: pr224-ordered-upstreams-failover-security
description: PR #224 (#218) ordered [[upstreams]] + scoped auth + cross-provider failover — auth-scope/credential-selection review posture and residual defects.
metadata:
  type: project
---

PR #224 (branch `amondnet/218-upstreams-failover`) adds ordered `[[upstreams]]`, a scoped `[auth]` credential map, shared physical-account state, and a cross-provider failover loop.

**Verified-safe (do not re-flag):**
- New `x-gateway-{upstream,model,upstream-model}` response headers stamp only config-derived names/models — no credential material. (src/proxy/failover.rs stamp_gateway_headers ~462)
- Failover remembered-failure responses are stamped for the provider that produced them (provider.clone() into remember_failure) — no cross-provider header mislabel/leak.
- `headers_for_route` strips client `authorization`/`x-api-key` for credential-injecting routes (src/proxy/failover.rs:448); inbound gate token stripped once in check_inbound_auth; `x-shunt-inbound-client` is stripped from inbound then re-inserted only for static_client injecting routes → no client spoofing.
- `account_key` store_family string-sniff fallback (`upstream.contains("codex"|"chatgpt")`) only affects health/quota STATE keying and only when store_family is None; the request path always sets store_family in resolve_pool_accounts → not a credential-selection path.
- `UpstreamConfig` and `AuthMap` have `deny_unknown_fields` (typos rejected) — tested in upstreams/tests.rs strict_auth_map_and_upstream_fields_reject_typos.

**Residual defects found (all minor, operator-misconfig-triggered, local-FS-trusted model):**
1. `accounts = []` (explicit empty subset) → empty account_scope (src/config/upstreams.rs:120) → resolve_pool_accounts treats empty scope as WHOLE-STORE scan (src/auth/shared.rs:336-337). An upstream declared to use zero accounts silently uses ALL stored OAuth accounts. No config-load rejection.
2. Inline `AccountConfig` (src/config.rs:1164) has NO `deny_unknown_fields` (unlike its siblings). A `token_env`/`credentials` typo on an inline account is silently dropped → resolve_{claude,chatgpt}_account falls back to the local store credential named after `account.name` (auth/mod.rs:141/184) instead of failing loud → wrong physical account.
3. Passthrough + cross-provider failover: a passthrough route keeps the client's `authorization`/`x-api-key` (headers_for_route src/proxy/failover.rs:443-445), so in a chain of ≥2 passthrough upstreams on different hosts the client credential is replayed to each host in one request lifecycle (new egress surface vs pre-failover single-upstream). Low confidence — passthrough is client-owned + unusual config.

Not re-flagged: upstream_error_body → client/tracing leak is pre-existing (see [[project_sentry-pii-egress]] / [[project_otel-pii-egress]]); crafted state/store file is out-of-scope by local-FS trust (rejected-findings a8ce52b24a7d5d28).
