# xAI account pool — design plan

Branch `feat/xai-account-pool`, worktree `/Users/rubenffuertes/repos/open-source/shunt-xai-pool`,
base upstream/main 0.55.0. Executed one ticket at a time from [`TICKETS.md`](TICKETS.md).

**Goal.** Pool several SuperGrok OAuth logins behind `auth = "xai_oauth"` (the built-in `grok`
provider: `kind = "responses"`, `https://cli-chat-proxy.grok.com/v1`) with the rotation, failover,
and cooldown the Claude, Codex, and Kimi pools already have. A user with only
`~/.shunt/xai-auth.json` changes nothing.

## 1. Design summary

**Data layout.** Named accounts live at `~/.shunt/accounts/xai/<name>.json`, overridable with
`SHUNT_XAI_ACCOUNTS_DIR`. A new `src/auth/xai/store.rs` mirrors `src/auth/kimi/store.rs:19-72` and
`src/auth/antigravity/store.rs:22-40,143-171`, built on the shared helpers:
`shared::default_accounts_dir` (`src/auth/shared.rs:221`), `scan_account_dir` (`:284`),
`write_account_file` (`:740`, dir `0700`, file `0600`, atomic), `validate_account_name` (`:203`).
A named file has **the same JSON shape as the legacy file**
(`{"tokens":{access_token,refresh_token,id_token?},"last_refresh"}`, `src/auth/xai/auth.rs:214-238`),
produced by one shared `tokens_value()`. `XaiAuthStore` (`src/auth/xai/auth.rs:72-165`) therefore
reads and refreshes both kinds unchanged. Pool identity is the file name (no `uuid`), as for Kimi
(`src/auth/kimi/store.rs:27-35`) and Antigravity.

**Legacy-file rule (chosen).** The legacy credential (`$SHUNT_XAI_AUTH_FILE`, default
`~/.shunt/xai-auth.json`, `src/auth/mod.rs:524-533`) is used **only when the provider's resolved
xAI pool is empty**: no inline `[[providers.<name>.accounts]]`, no `account`/`accounts` scope, and
`$SHUNT_XAI_ACCOUNTS_DIR` missing or holding no valid `<name>.json`. It is never a pool member.
Once the pool is non-empty, shunt does not read it.

- Precedent: Codex (`src/adapters/responses/mod.rs:375-377`, documented at `src/config.rs:1804-1807`)
  and Antigravity (`src/adapters/gemini/mod.rs:189-197`) use this exact rule.
- A reserved pool member was rejected: it needs a reserved name that can collide with a user's
  `<name>.json`, plus a new identity rule, and it would move zero-config users off the
  `resolve_credential` path they use today (`src/auth/mod.rs:109-117`).
- Trap: adding one named account silently drops the legacy login from service. Mitigation: the
  shapes match, so migration is `mv ~/.shunt/xai-auth.json ~/.shunt/accounts/xai/<name>.json`, and
  `shunt login xai --name` prints that hint when the legacy file exists (T2). The routed inbound
  Codex endpoint applies the same rule (T9b), so the `mv` does not break it.
- Testable at the gateway: T9 tests 1–3.

**Login.** `shunt login xai --name <name>` runs the same device flow and writes the named file,
mirroring `auth::antigravity::login::run_named` (`src/auth/antigravity/login.rs:128-142`).
`shunt login xai` without `--name` keeps today's code path and output (`src/auth/xai/login.rs:52-100`).
`--mode` / `--long-lived` get xAI-specific rejections in place of `src/main.rs:380-387`.

**Config (no new keys).**
- `[[upstreams]]` form: `auth = { mode = "xai_oauth", account = "…" }` or `accounts = [...]` (not
  both). This reuses `absorb_oauth_scope` (`src/config/upstreams.rs:169-207`); today the variant is
  the mode-only `XaiOauth {}` (`:103`, `:153`).
- Legacy form: `[[providers.grok.accounts]]` is admitted by adding `XaiOauth` to the accounts gate
  (`src/config.rs:4608-4619`, error text `:2736`).
- `ProviderConfig::store_family` / `resolve_pool_accounts` get an `XaiOauth` arm
  (`src/config.rs:3157-3207`) mapping to a new `StoreFamily::Xai` (`src/accounts.rs:27-32`).
- The bearer guard (`src/config.rs:4831-4849`: kind `responses`, https, x.ai/grok.com host) is
  untouched. It runs for every provider after the accounts gate, so every pooled account is held
  to it.

**Request path: xAI rides the existing Codex Responses pool, with no fourth copy (#387).**
- The three dispatch gates in `src/adapters/responses/mod.rs` (`:302` streaming and non-streaming,
  `:588` committed chain) become `ChatgptOauth | XaiOauth`. They call
  `provider.resolve_pool_accounts()` (`src/config.rs:3169`, already used by
  `src/adapters/gemini/mod.rs:191-194`) instead of the hard-coded codex store arguments
  (`:317-324`, `:359-366`, `:593-600`).
- Inside `src/adapters/responses/pool.rs` the family is decided **per account** by
  `AccountConfig::store_family`. `resolve_pool_accounts` always stamps it (`src/auth/shared.rs:427-428`),
  and the re-probe gate already dispatches the same way (`src/accounts.rs:1079`).
  - `resolve_or_cooldown` (`pool.rs:1574-1605`): an Xai account resolves through a new
    `auth::resolve_xai_account`, which mirrors `resolve_kimi_account` (`src/auth/mod.rs:331-377`):
    `token_env`, then `credentials`, then the store path, into `XaiAuthStore::get_valid`.
  - `classify_first` (`pool.rs:1885-1924`): an Xai account is classified with
    `accounts::classify_xai` and skips the Codex-only 400 model-refusal body read (`pool.rs:1831-1863`).
  - `classify_xai` never returns `RefreshRetry`, so `force_refresh_or_cooldown` (`:1622`) and
    `classify_retry` (`:1942`) are never reached for xAI.
- Gateway-owned error wording follows the provider's auth. The hard-coded `AuthMode::ChatgptOauth`
  at `pool.rs:210, 353, 1033, 1074` and `mod.rs:646` is replaced. "all Codex OAuth accounts failed…"
  (`pool.rs:596, 1435`, `mod.rs:756`) becomes family-specific.
- Error shapes stay the same: the Anthropic envelope via `mapped_upstream_error` / `transport_error` /
  `own_error` (`src/adapters/responses/error.rs:145-158`), and the existing xAI 401/403 texts
  (`error.rs:96-118`). The inbound Codex endpoint (`inbound.rs`) only ever sees Chatgpt-stamped
  accounts, so it is unchanged.
- WebSocket is never enabled for xAI (`src/config.rs:5712-5716`). Re-probing excludes non-Chatgpt
  families (`src/accounts.rs:1079`).
- Streaming semantics are those of pooled Codex: the response commits before the scan
  (`pool.rs:178-235`). An empty pool falls back to `http_events_stream` inside the committed stream,
  the path every unpooled `chatgpt_oauth` turn already takes. Wire output is the same SSE. Pooled
  xAI streams gain the existing `event: account` frame, and non-streaming responses gain
  `x-shunt-account`, as pooled Codex does.
- Routed inbound Codex endpoint (`[[server.codex_endpoint.routes]]` → an `xai_oauth` provider,
  T9b): `forward_codex_routed` (`src/adapters/responses/inbound_routed.rs:55-62`) calls
  `provider.resolve_pool_accounts()`. If the pool is non-empty, it resolves the first account in pool
  selection order (`AccountPool::select_order_with_cap`, `src/accounts.rs:805`), which honors the
  configured scope, `priority`, `disabled`, and the admin pause, and sends once with no rotation and
  no cooldown. An empty pool falls back to `resolve_credential`, as before. This mirrors the
  empty-pool fallback in `src/adapters/gemini/mod.rs:189-197`. Gateway errors keep the OpenAI
  Responses shape, because `src/codex_endpoint.rs:122-126` re-shapes every `AdapterError` there.

**Refresh locking: the store does not change.** Each account gets its own `XaiAuthStore` over its
own path. The refresh and the rotated-pair writeback run in a detached task that owns the
single-flight lock for that file, and a waiter re-reads **its own** file (`src/auth/xai/auth.rs`).
This keeps single-flight per file and stays safe when the caller is cancelled. The lock is per
credential file (`auth::shared::refresh_lock`, shared with Codex; see Q5), so refreshes on different
accounts run in parallel; the original process-wide lock, which serialized them as Kimi does
(`docs/m15-kimi-oauth.md` §5), was replaced during code review. A write only reaches the path its store was built with, so two
accounts cannot cross-write; T6 adds a concurrent two-account refresh test. Pool resolution does not
take `AccountPool::refresh_lock`, matching Codex (`pool.rs:1564-1573`, test `pool.rs:3740`).

**Refresh client (review decision D2).** `resolve_xai_account` builds each pooled `XaiAuthStore`
with the redirect-hardened `crate::auth::shared::token_refresh_client()`, not the caller's client,
as `resolve_kimi_account` does (`src/auth/mod.rs:336-353`). `state.http_client` carries a redirect
policy (`codex_identity_redirect_policy`, `src/adapters/responses/request.rs:269`) that only guards OpenAI/ChatGPT origins. A 307/308
from the token endpoint re-POSTs the form body, which carries the refresh token
(`refresh_tokens`, `src/auth/xai/auth.rs:271`). `token_refresh_client()` refuses any hop to non-loopback `http://`
(`is_safe_refresh_url` / `token_refresh_client`, `src/auth/shared.rs:123,175`); T6 tests that. The legacy `resolve_credential` arm
(`src/auth/mod.rs:109-117`) is unchanged; §5 lists the deferred hardening.

**Failure classification.** `classify_xai` is `classify_codex` (`src/accounts.rs:3816-3830`) with
only 401 and 402 changed to Rotate. Cooldowns come from `rotate_cooldown` (`pool.rs:1524-1535`) and
reasons from the shared `rotation_reason` (`src/accounts.rs:3747-3765`), which already labels a 402
`membership`. No label logic is added, and nothing shared changes.

| Outcome | `classify_xai` | Pool action | Cooldown | Rotation reason |
| :-- | :-- | :-- | :-- | :-- |
| 2xx | Relay | mark healthy, relay | — | — |
| 401 | Rotate | cool down and rotate; no same-account refresh retry | 30 s | `auth` |
| 402 | Rotate | cool down and rotate | 30 s | `membership` |
| 403 | Relay | relay the mapped error (existing xAI 403 text), `mark_healthy(false)`; no rotation, no cooldown | — | — |
| 429 | Rotate | cool down and rotate | `retry-after` clamped 1–3600 s, default 60 s | `rate_limit` |
| 5xx | Rotate | cool down and rotate | 30 s | `server_error` |
| other 4xx (400, 404, 413, …) | Relay | relay the mapped error, `mark_healthy(false)`; body never read | — | — |
| credential resolution fails (missing or corrupt file, refresh 400/401 `invalid_grant`, refresh 403 tier gate, refresh 5xx, unset `token_env`) | — | cool down and rotate | 5 min | `auth` |
| transport error | — | cool down and rotate | 30 s | `transport` |
| TTFB timeout | — | terminal 504 (unchanged) | — | — |
| exhausted after ≥1 response | — | relay the last response, mapped with `xai_oauth` (existing 401/403 texts) | — | `exhausted` |
| exhausted with no response | — | `all xAI OAuth accounts failed before receiving an upstream response` (BeforeHeaders; on `/v1/messages` the chain answers `all upstreams failed (N attempted)`, `src/proxy/failover/chain.rs:261,297`) | — | `exhausted` |

- **401 rotates.** `XaiAuthStore` has no forced-refresh entry point; expiry is the JWT `exp`
  (`src/auth/xai/auth.rs:92-109`). This follows the Kimi pool (`src/adapters/anthropic/mod.rs:722-736`).
- **402 rotates.** A payment/entitlement refusal is account-specific and persistent, Kimi's 402
  membership case (`src/accounts.rs:3790-3806`, `docs/m15-kimi-oauth.md` §8). When every account
  returns 402, the client sees 502 (`client_facing_status` maps it), not the upstream 402; this holds
  for a single-account pool too.
- **403 relays (review decision D1, reversed in round 2).** This is the same as `classify`,
  `classify_codex`, `classify_kimi`, and `classify_antigravity`; T5 pins it. Three reasons:
  - The documented xAI tier gate surfaces at **token refresh** (`docs/m6-xai-provider.md:116`,
    `src/auth/xai/auth.rs:295-311`). The pool already cools a resolution failure down for 5 minutes
    (`auth`) and rotates, so a gated account leaves the rotation that way.
  - An **inference** 403 can be a content-policy or model-gating refusal
    (`src/adapters/responses/error.rs:288-289`). Rotating would re-send a policy-flagged request to
    every one of the user's accounts, which risks getting all of them flagged.
  - Relaying changes nothing shared: no new label and no edit to `rotation_reason`.
  - Follow-up: capture a live CLI-proxy 403 body (§6 step 5). If it carries a distinct tier-gate
    code, rotate on that code only.

**Admin and usage (read-only).** `XaiOauth` joins the auth-mode filters in `GET /admin/api/pool`
(`src/admin/mod.rs:1179-1232`), `PATCH /admin/api/pool/{provider}/accounts/{ref}` (`:1390-1435`), and
`GET /usage` (`src/usage.rs:449-489`). Each new arm calls `provider.resolve_pool_accounts()` rather
than pasting another copy of the five-argument call. There are no provisioning routes (Kimi
precedent, `docs/m15-kimi-oauth.md` §9). Plan lookup already returns nothing for non-Claude/Codex
modes (`src/admin/plan.rs:402-405`).

**Docs.** A new `docs/m18-xai-multi-account.md` follows the repo's split between base provider specs
and pool specs (Codex M2 → M10, Anthropic M8). `m6` gets a pointer. Then come the site, README, and
locale updates.

## 2. Requirement → where it is met

| Requirement | Ticket(s) |
| :-- | :-- |
| 1. Zero-config compatibility | legacy rule (T9 dispatch, T9 tests 1–3; T9b on the routed inbound endpoint); `shunt login xai` unchanged (T2) |
| 2. Config shape like `kimi_oauth`; only new env var `SHUNT_XAI_ACCOUNTS_DIR` | T1, T3, T4 |
| 3. Off-origin guard for every account | guard untouched; T3 tests prove it with accounts present |
| 4. Per-account single-flight, atomic `0600`, no cross-write | store reused per path; T1 (perms), T6 (two-account refresh, redirect refusal) |
| 5. Failure classification | T5 (`classify_xai`), T7b/T8 (pool behavior, including 403 relay) |
| 6. `/admin/api/pool`, `/usage` | T10b |
| 7. Error shapes and streaming unchanged | T7a (error wording only, plus xAI-wording tests), T9 streaming tests, T9b (OpenAI shape on the inbound endpoint) |
| 8. Tests | every code ticket; T10a isolates existing fixtures from the real xAI store |
| 9. Docs | T11–T15 |

## 3. Test strategy

- xai_oauth validation demands https and an x.ai/grok.com host, with **no loopback exception**
  (`src/config.rs:4831-4849`), unlike claude/chatgpt/kimi. No wiremock inference test can go through
  the gateway (`src/server.rs:189-192` always validates). Therefore:
  - Pool behavior is tested at module level (`src/adapters/responses/pool_xai_tests.rs`, T7a/T7b/T8).
    The route points at the loopback `openai` provider and uses Xai-stamped accounts with
    `credentials` files that hold far-future JWTs, so there are no env writes and no refresh.
  - **Fidelity gap.** That route's provider auth is `api_key`, so the pool's gateway-owned wording
    in those tests is the generic mapping. The test file's `//!` says so. The xAI wording is asserted
    separately in three places:
    - T7a: `provider_auth` on the built-in `grok` provider, and `exhausted_error` with
      `AuthMode::XaiOauth`.
    - `src/adapters/responses/error.rs:257-301`: existing `mapped_upstream_error` tests with
      `XaiOauth` for 401 and 403.
    - T9: the gateway.
  - Gateway tests (`tests/xai_multi_account.rs`, T9) cover dispatch and the legacy rule, using only
    failures that never leave the process (missing or `{}` credential files).
  - The routed inbound selection (T9b) is tested at unit level on an `xai_oauth` provider with an
    https grok host; it sends nothing.
- Every new test points `SHUNT_XAI_ACCOUNTS_DIR` / `SHUNT_XAI_AUTH_FILE` at temp paths. `src/` tests
  hold `crate::auth::xai::store::TEST_ENV_LOCK`; `tests/` use `common::set_env` / `common::env_lock`
  (`tests/common/mod.rs`, `tests/AGENTS.md`).
- Existing fixtures: once T10b lands, every pool listing scans the built-in `grok` store. T10a lands
  first and isolates the existing fixtures (`tests/common/mod.rs` guards, `src/usage/tests.rs`)
  without changing any assertion.
- T9 writes the two legacy-path tests **first** and runs them on the pre-dispatch tree. They assert
  the exact text `xAI auth not found; run shunt login xai` and must still pass after the change.

## 4. Out of scope

- Refactoring the Claude/Codex/Kimi pool paths or deduplicating the existing five-argument
  `resolve_pool_accounts` copies (#387).
- A forced refresh, or a same-account retry, on an upstream 401.
- The `needs_relogin` dashboard mark for xAI. Kimi parity: resolution failures only cool down.
- Admin provisioning routes (`/admin/api/accounts/xai`).
- Quota windows, out-of-band usage polling, and `max_utilization` caps for xAI; caps stay inert, as
  for Kimi.
- Rotation, failover, or cooldown on the routed inbound Codex endpoint. T9b only picks one account.
- Hardening the legacy `xai_oauth` refresh client, and refusing cross-origin HTTPS redirects in
  `token_refresh_client` (deferred to the maintainer; see §5).
- Model discovery (`src/discovery/upstream.rs` returns `None` for `xai_oauth`).
- Relaxing the xai_oauth https/host guard.
- The Grok CLI observation lane (`src/auth/observation.rs`).
- WebSocket transport for xAI.
- Codex-flavored `tracing` strings inside the shared pool loop (e.g. "ChatGPT OAuth upstream request
  failed"), which are cosmetic.
- Regenerating `wiki/`.

## 5. Review decisions (2026-10-08)

Design review by GPT-6-Astra and Fable; the decisions are the team lead's.

| # | Question | Decision |
| :-- | :-- | :-- |
| Q1 | Is every CLI-proxy 403 a tier gate? | **No: relay (D1, reversed in round 2).** Reasoning in §1. The documented gate surfaces at refresh, which already cools down and rotates. Follow-up: capture a live 403 body (§6 step 5) and rotate only on a distinct tier-gate code. |
| Q2 | `needs_relogin` for dead accounts | **Defer.** `refresh_error_message` already separates 400/401 (terminal) from 403/5xx, so a typed store error is a small follow-up, not a redesign. |
| Q3 | Routed inbound endpoint vs the `mv` advice | **Fixed (D3, T9b).** First account in selection order, scope honored, single send. |
| Q4 | Refresh HTTP client | **Pooled accounts use `token_refresh_client()` (D2, T6).** Deferred for the maintainer, per `src/AGENTS.md` "ask before changing refresh semantics": hardening the legacy `resolve_credential` arm, and making `token_refresh_client` refuse cross-origin HTTPS redirects (today it refuses only non-loopback `http://`, `src/auth/shared.rs:123`). |
| Q5 | Global refresh lock | **Accept.** Refreshes are rare (JWT `exp` with a 5-minute buffer), and one lock keeps single-flight simple. Astra's note stands as a follow-up: a stalled refresh blocks every account until the client timeout. **Revised 2026-10-10:** code review found the refresh client has no timeout, so the stall had no bound. The store now uses the per-file lock shared with Codex and a 30-second refresh limit. |
| Q6 | Identity coalescing via JWT `sub` | **Keep filename identity.** An unverified claim could merge two paid logins into one pool member. |
| Q7 | Loopback exception in the guard | **Keep the guard.** Module-level pool tests plus in-process gateway tests cover the paths. |
| Q8 | Named-account error text | **Accept.** The store messages are shared with the legacy file, and the pool's `tracing::warn!` names the account. |
| Q9 | Tests reading the real store | **Fixed (D6, T10a).** Existing fixtures are isolated before T10b adds the scan. |
| Q10 | 403 label | **Moot.** A 403 relays, so there is no rotation and no label. 402 keeps the shared `membership` label. |

## 6. Manual verification (operator, after T10b and T9b; not a ticket)

1. `shunt login xai --name a` and `shunt login xai --name b` with the two SuperGrok accounts. Check
   `~/.shunt/accounts/xai/{a,b}.json` are `0600` and the directory is `0700`.
2. **Now** run the full gate (`cargo test --all-features --workspace`): a real populated
   `~/.shunt/accounts/xai` is what would expose a test fixture still reading the operator's store.
3. `shunt run`; send two non-streaming requests to a `grok-*` route with different
   `x-claude-code-session-id` values. Check `x-shunt-account` shows each account.
4. `GET /admin/api/pool` lists `grok` with `a` and `b`.
5. Capture a live CLI-proxy 403: send a request with an account or model the subscription does not
   cover. Save the status and body (redact tokens) to `docs/notes/`. If the body carries a distinct
   tier-gate code, the Q1 follow-up rotates on that code only; every other 403 keeps relaying.
6. Rename `a.json` away; the next request is served by `b`. Remove both; the gateway falls back to
   `~/.shunt/xai-auth.json` if present.
