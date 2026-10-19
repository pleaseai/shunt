# TICKETS — xAI account pool

Read [`PLAN.md`](PLAN.md) first; it carries the design, the legacy-file rule, and the
classification table. Each ticket below is one implementer pass.

## Rules for every ticket

- Work only in your checkout of this repository. The shell cwd may reset, so run every command from
  the repository root (`cd "$(git rev-parse --show-toplevel)" &&`).
- Touch **only** the files the ticket lists. If something else seems to need a change, stop and
  report instead.
- Never delete or weaken an existing test. Never edit an existing test unless the ticket says so.
- Tests never read or write the real `~/.shunt`. Point `SHUNT_XAI_ACCOUNTS_DIR` and
  `SHUNT_XAI_AUTH_FILE` at fresh temp paths.
  - In `src/` tests: hold `crate::auth::xai::store::TEST_ENV_LOCK` (created in T1) for the whole
    test, and set variables with `crate::auth::shared::EnvVarGuard::set(KEY, value)` (key must be a
    `&'static str`; declare the guard *after* taking the lock).
  - In `tests/`: use `common::set_env(..)` / `common::env_lock()` bound to a named local
    (`let mut vars = ...`). Never call `std::env::set_var` / `remove_var` there —
    `tests/env_lock_coverage.rs` fails the build.
- No network in tests: wiremock or pure fixtures only.
- Line numbers are HEAD's (e9b85be). Earlier tickets shift them, so locate each edit by the quoted
  text or function name; a line number is only a hint.
- New pub items get a doc comment saying *why*, in the style of the Kimi/Antigravity code they mirror.
  Error strings follow the Kimi wording with "xAI".
- Do not commit or push. When done, report: files changed, tests added (names), and the gate output.
- **Standard gate** (run in order; every step must pass):

```bash
cargo fmt --all
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --workspace
```

## Dependency graph

| id | title | depends_on | can_run_parallel_with |
| :-- | :-- | :-- | :-- |
| T1 | xAI named-account store | — | — |
| T2 | `shunt login xai --name` | T1 | T3, T6 |
| T3 | `StoreFamily::Xai`, provider resolution, validation | T1 | T2, T6 |
| T4 | `[[upstreams]]` `xai_oauth` account scope | T3 | T2, T6, T10a |
| T5 | `classify_xai`: 401/402 rotate, 403 relays (shared `rotation_reason` untouched) | T3 | T2, T4, T6, T10a |
| T6 | `resolve_xai_account` (hardened refresh client) + two-account isolation + redirect refusal | T1 | T2, T3, T4, T5, T10a |
| T7a | Responses pool: per-family resolution, classification, wording; xAI-wording tests | T3, T5, T6 | T4, T10a, T10b |
| T7b | Responses pool: 403-relay, 401-rotation, unresolvable-account tests | T7a | T9, T9b, T10a, T10b |
| T8 | Responses pool: remaining xAI classification tests | T7b | T9, T9b, T10a, T10b |
| T9 | Dispatch gates + gateway tests (legacy rule) | T7a | T7b, T8, T9b, T10a, T10b |
| T9b | Routed inbound Codex endpoint: first xAI pool account | T6, T7a | T7b, T8, T9, T10a, T10b |
| T10a | Store isolation for existing fixtures (no production code) | T3 | T4–T9b |
| T10b | Admin pool, PATCH, `/usage` + their tests | T10a | T7a–T9b |
| T11 | Docs: `docs/` (m18, m6, running, upstreams-failover, m11) | T2, T4, T9, T9b, T10b | — |
| T12 | Docs: site (English), README.md, grok blueprint | T11 | — |
| T13 | Docs: Korean (`ko`) | T12 | T14, T15 |
| T14 | Docs: Japanese (`ja`) | T12 | T13, T15 |
| T15 | Docs: Simplified Chinese (`zh-cn`) | T12 | T13, T14 |

"Parallel" means the files do not overlap. Run tickets that edit the same file one after another.

---

## T1 — xAI named-account store

- **depends_on:** —
- **files:** `src/auth/xai/store.rs` (new, primary), `src/auth/xai/auth.rs`, `src/auth/xai/mod.rs`

**Instructions**

1. `src/auth/xai/auth.rs`: pull the JSON builder out of `write_tokens` (lines 214-238). This is a
   pure refactor, so file content and behavior must not change. Add this function directly above
   `write_tokens`:

   ```rust
   /// The credential-file JSON: `{"tokens": {...}, "last_refresh": "<ISO8601>"}`.
   /// Shared by the legacy single-account file ([`write_tokens`]) and the named
   /// account store ([`super::store::store_oauth_tokens`]), so [`XaiAuthStore`]
   /// reads and refreshes either one.
   pub(crate) fn tokens_value(
       access_token: &str,
       refresh_token: &str,
       id_token: Option<&str>,
   ) -> Value {
       let mut tokens = serde_json::Map::new();
       tokens.insert("access_token".to_string(), json!(access_token));
       tokens.insert("refresh_token".to_string(), json!(refresh_token));
       if let Some(id_token) = id_token {
           tokens.insert("id_token".to_string(), json!(id_token));
       }
       json!({
           "tokens": Value::Object(tokens),
           "last_refresh": format_iso8601(SystemTime::now()),
       })
   }
   ```

   In `write_tokens`, keep the doc comment and the `create_dir_all` block, and replace everything
   after that block with `write_auth_file_atomic(path, &tokens_value(access_token, refresh_token, id_token))`.

2. `src/auth/xai/mod.rs`: add `pub mod store;` after `pub mod login;`.

3. Create `src/auth/xai/store.rs`, mirroring `src/auth/kimi/store.rs:1-75` and
   `src/auth/antigravity/store.rs:1-40`:

   ```rust
   //! Shunt-owned xAI subscription account files.
   //!
   //! Each account is a named file at `~/.shunt/accounts/xai/<name>.json` (or
   //! `$SHUNT_XAI_ACCOUNTS_DIR/<name>.json`) in the same JSON shape as the
   //! legacy single-account file (`~/.shunt/xai-auth.json`, see
   //! [`crate::auth::default_xai_auth_path`]), so [`super::auth::XaiAuthStore`]
   //! reads and refreshes either one. Named logins never write the legacy file,
   //! and the legacy login never writes here.

   use std::{io, path::PathBuf};

   use crate::auth::shared;
   use crate::config::AccountConfig;

   // Name validation and born-private write are provider-agnostic, so they live
   // in `auth::shared` and every store calls them — only the env var and subdir
   // differ here.
   pub use crate::auth::shared::validate_account_name;

   pub fn default_accounts_dir() -> PathBuf {
       shared::default_accounts_dir("SHUNT_XAI_ACCOUNTS_DIR", "xai")
   }

   pub fn account_path(name: &str) -> PathBuf {
       default_accounts_dir().join(format!("{name}.json"))
   }

   /// Return store-managed accounts in deterministic name order. shunt reads no
   /// verified upstream account identifier from an xAI login, so every scanned
   /// entry gets no `uuid` and falls back (via `accounts::account_key`) to its own
   /// file name as its pool identity — the Kimi and Antigravity stores' rule.
   pub fn scan_accounts() -> io::Result<Vec<AccountConfig>> {
       shared::scan_account_dir(&default_accounts_dir(), |_path| None)
   }

   /// Store a freshly issued xAI device-flow login as a named account, in the
   /// shape [`super::auth::XaiAuthStore`] reads and refreshes.
   pub fn store_oauth_tokens(
       name: &str,
       access_token: &str,
       refresh_token: &str,
       id_token: Option<&str>,
   ) -> anyhow::Result<PathBuf> {
       validate_account_name(name)?;
       let access_token = access_token.trim();
       let refresh_token = refresh_token.trim();
       if access_token.is_empty() || access_token.chars().any(char::is_whitespace) {
           anyhow::bail!("xAI access token must be one non-empty value without whitespace");
       }
       if refresh_token.is_empty() || refresh_token.chars().any(char::is_whitespace) {
           anyhow::bail!("xAI refresh token must be one non-empty value without whitespace");
       }
       let id_token = id_token.map(str::trim).filter(|token| !token.is_empty());
       let value = super::auth::tokens_value(access_token, refresh_token, id_token);
       let path = account_path(name);
       shared::write_account_file(&path, &value)?;
       Ok(path)
   }

   #[cfg(test)]
   pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
   ```

**Tests to add** (in `store.rs` `#[cfg(test)] mod tests`). Use a `temp_dir(tag)` helper like
`kimi/store.rs:85-94`, and `fn jwt(exp: u64) -> String` like `xai/auth.rs:318-324`. Every test that
touches env holds `TEST_ENV_LOCK` and uses `shared::EnvVarGuard`.

- `validates_account_names`: copy of `kimi/store.rs:96-105`.
- `oauth_tokens_round_trip_in_the_legacy_file_shape`: `store_oauth_tokens("primary", "access",
  "refresh", Some("id"))`. Assert the returned path equals `account_path("primary")`, and that the
  file JSON has `tokens.access_token == "access"`, `tokens.refresh_token == "refresh"`,
  `tokens.id_token == "id"`, and a string `last_refresh`.
- `a_named_account_file_is_readable_by_the_xai_auth_store`: store with access token `jwt(4_000_000_000)`.
  Then `crate::auth::xai::auth::XaiAuthStore::new(path, reqwest::Client::new()).get_valid().await`
  returns that same token (no refresh, no network).
- `oauth_tokens_reject_blank_tokens`: an empty access token, an empty refresh token, and
  `"has space"` as access token are each `Err`.
- `oauth_tokens_reject_invalid_names`: `"../escape"` and `"Primary"` are `Err`.
- `written_file_and_directory_are_private` (`#[cfg(unix)]`): file mode `0o600`, directory mode
  `0o700` (copy of `kimi/store.rs:145-165`).
- `scan_accounts_skips_non_account_files_and_uses_filename_identity`: store `zeta` and `alpha`, and
  write `ignore.txt` and `Bad.json`. Names are `["alpha", "zeta"]`, every `uuid` is `None`, and every
  `store_entry` is `true`.
- `scan_accounts_missing_dir_is_empty`.
- `named_login_never_writes_the_legacy_file`: set `SHUNT_XAI_ACCOUNTS_DIR=<tmp>/accounts` and
  `SHUNT_XAI_AUTH_FILE=<tmp>/legacy.json`, then call `store_oauth_tokens`. Assert
  `crate::auth::default_xai_auth_path() == <tmp>/legacy.json` and that the file does not exist.

**Acceptance**

```bash
cargo test --all-features --lib auth::xai
```

The existing `auth::xai::auth` tests (`write_tokens_round_trips_and_rotates_refresh`,
`written_file_is_private`, `concurrent_get_valid_single_flights_the_refresh`, …) pass unchanged.
Then run the standard gate.

---

## T2 — `shunt login xai --name <account-name>`

- **depends_on:** T1
- **files:** `src/auth/xai/login.rs` (primary), `src/main.rs`

**Instructions**

1. `src/auth/xai/login.rs` (mirror `src/auth/antigravity/login.rs:116-142`):
   - Imports: add `use std::path::Path;` and `use super::store;`.
   - Move lines 56-74 of `run` (the client, `request_device_code`, the three `println!`s, and
     `poll_for_tokens`) into a new private function. Keep the text verbatim:

     ```rust
     /// The device-code flow shared by [`run`] and [`run_named`]: request a code,
     /// print the verification URL and user code, and poll until the user
     /// approves. `poll_for_tokens` already rejects a response with no
     /// refresh token.
     async fn device_login() -> anyhow::Result<super::auth::TokenResponse> { … }
     ```

   - Move lines 89-98 (the two success `println!`s) into
     `fn report_login(path: &Path, access_token: &str)`.
   - `run(provider)` becomes: the unchanged `provider != "xai"` check; `let tokens = device_login().await?;`;
     the unchanged `refresh_token` extraction and `default_xai_auth_path()` + `write_tokens` +
     `with_context`; then `report_login(&path, &tokens.access_token); Ok(())`. Printed output must
     stay byte-identical.
   - Add:

     ```rust
     /// `shunt login xai --name <account-name>`: the same device flow as [`run`],
     /// persisted as a named account file (see [`super::store`]) so the account can
     /// join an xAI account pool. The legacy single-account file is never written
     /// here. Mirrors `auth::antigravity::login::run_named`.
     pub async fn run_named(name: &str) -> anyhow::Result<()> {
         store::validate_account_name(name)?;
         let tokens = device_login().await?;
         let refresh_token = tokens
             .refresh_token
             .as_deref()
             .ok_or_else(|| anyhow!("xAI did not return a refresh_token; cannot persist login"))?;
         let path = store::store_oauth_tokens(
             name,
             &tokens.access_token,
             refresh_token,
             tokens.id_token.as_deref(),
         )
         .with_context(|| format!("failed to write xAI credentials for account {name:?}"))?;
         report_login(&path, &tokens.access_token);
         if let Some(notice) = legacy_notice(&default_xai_auth_path(), &store::default_accounts_dir()) {
             println!("\n{notice}");
         }
         Ok(())
     }

     /// The migration hint [`run_named`] prints when the legacy single-account
     /// file exists: an xAI provider reads that file only while its account pool
     /// is empty (docs/m18-xai-multi-account.md), so the first named login takes
     /// it out of service. Both files share one format, so moving it keeps it.
     fn legacy_notice(legacy: &Path, accounts_dir: &Path) -> Option<String> {
         legacy.exists().then(|| {
             format!(
                 "Note: while any named xAI account exists, shunt pools the accounts in {} and no longer reads {}.\n\
                  To keep that older login in the pool, stop shunt and move it into the store:\n    \
                  mv {} {}/<account-name>.json",
                 accounts_dir.display(),
                 legacy.display(),
                 legacy.display(),
                 accounts_dir.display(),
             )
         })
     }
     ```

   - Module doc (lines 1-10): add one sentence saying `--name` writes
     `~/.shunt/accounts/xai/<name>.json` via [`super::store`] instead of the legacy file.
2. `src/main.rs`:
   - Replace the two `"xai"` arms at lines 380-387 with:

     ```rust
     "xai" if long_lived => {
         anyhow::bail!(
             "--long-lived is not supported for `shunt login xai`; xAI OAuth tokens are always refreshable"
         )
     }
     "xai" if mode.is_some() => {
         anyhow::bail!(
             "--mode is not supported for `shunt login xai`; xAI OAuth tokens are always refreshable"
         )
     }
     "xai" => match name {
         Some(name) => runtime()?.block_on(shunt::auth::xai::login::run_named(name)),
         None => runtime()?.block_on(shunt::auth::xai::login::run(provider)),
     },
     ```

   - `name` arg doc (lines 103-104): `(`claude`, `codex`, `kimi`, `antigravity`, and `xai` only)`.

**Tests to add**

- `login.rs` `mod tests`:
  - `legacy_notice_appears_only_when_the_legacy_file_exists`: with a temp dir, the legacy path
    absent gives `None`; after writing the file, the text contains the legacy path, the accounts dir,
    and `"mv "`.
  - `run_named_rejects_an_invalid_name_before_any_network_call`: `run_named("Not Valid").await` is
    `Err` whose text contains `must match [a-z0-9-]+`.
- `main.rs` `mod tests` (line 964): `xai_login_rejects_long_lived_and_mode_with_xai_wording`.
  `login("xai", None, true, None, false, None)` errors with text containing
  `` --long-lived is not supported for `shunt login xai` ``; and
  `login("xai", Some("primary"), false, Some(LoginMode::Import), false, None)` errors with text
  containing `` --mode is not supported for `shunt login xai` ``.

**Acceptance**

```bash
cargo test --all-features --lib auth::xai::login
cargo test --all-features --bin shunt xai_login
```

The existing `login.rs` tests pass unchanged. Then run the standard gate.

---

## T3 — `StoreFamily::Xai`, provider resolution, validation

- **depends_on:** T1
- **files:** `src/config.rs` (primary), `src/accounts.rs` (enum only), `src/auth/shared.rs` (one arm)

**Instructions**

1. `src/accounts.rs:27-32`: add `Xai,` after `Antigravity,` in `pub enum StoreFamily`.
2. `src/auth/shared.rs:611`: change it to
   `family: StoreFamily::Kimi | StoreFamily::Antigravity | StoreFamily::Xai,`. In the comment just
   below, say Kimi, Antigravity, and xAI have no verified identity source.
3. `src/config.rs`:
   - a. `AuthMode::XaiOauth` doc (2206-2208): add "or per-account in
     `~/.shunt/accounts/xai/<name>.json` (`shunt login xai --name <account-name>`), which form an
     account pool".
   - b. `accounts` field doc (1799-1802): list `xai_oauth` (xAI) with the other modes.
   - c. `account_scope` doc (1804-1809): "For `chatgpt_oauth` and `xai_oauth`, an empty store still
     falls back to the single-account file (`~/.codex/auth.json` / `~/.shunt/xai-auth.json`) as before."
   - d. `retry` doc (1846-1854): the single-credential Responses path becomes "`api_key`, or an
     unpooled `chatgpt_oauth`/`xai_oauth` provider"; the pools become
     "`claude_oauth`/`chatgpt_oauth`/`kimi_oauth`/`xai_oauth` account pools".
   - e. Error text (2736):
     `"providers.{provider}.accounts requires auth = \"claude_oauth\", \"chatgpt_oauth\", \"kimi_oauth\", \"xai_oauth\", or \"antigravity_oauth\""`.
   - f. `store_family` (3157-3167): add `AuthMode::XaiOauth => Some(StoreFamily::Xai),`.
   - g. `resolve_pool_accounts` (3169-3207): import `xai` in `use crate::auth::{antigravity, claude, codex, kimi, xai};`
     and add the arm
     `AuthMode::XaiOauth => ("xAI", xai::store::default_accounts_dir(), xai::store::scan_accounts),`
     before `_ => unreachable!()`.
   - h. Accounts gate (4608-4619): add `| AuthMode::XaiOauth` to the `matches!`. Do **not** touch the
     `XaiOauth` guard at 4824-4849.

**Tests to add** (in the `config.rs` test module, right after
`xai_oauth_accepts_grok_com_host_but_still_rejects_other_origins`):

- `xai_provider_resolves_named_pool_and_leaves_the_legacy_file_to_an_empty_pool`: mirror
  `antigravity_provider_resolves_named_pool_and_preserves_singleton` (config.rs:6554-6604).
  - Hold `crate::auth::xai::store::TEST_ENV_LOCK`, set `SHUNT_XAI_ACCOUNTS_DIR` to a fresh temp dir,
    and take `provider = Config::default().providers.remove("grok").unwrap()`.
  - Assert `provider.auth == AuthMode::XaiOauth` and `store_family() == Some(StoreFamily::Xai)`.
  - The pool resolves empty with the dir missing, and still empty once the dir is created.
  - Write `primary.json` and `backup.json` (content `{}`). Names are `["backup", "primary"]`, and all
    have `store_entry`, `store_family == Some(StoreFamily::Xai)`, and `uuid == None`.
  - Set `account_scope = ["primary"]` plus one inline account
    (`name "inline"`, `credentials = <dir>/inline.json`, `priority 2`, `disabled true`). Resolves to
    `[primary (store_entry), inline (!store_entry, family Xai, priority 2, disabled)]`.
  - Clearing `account_scope` resolves to 1 account.
  - `account_scope = ["missing"]` errors with text containing
    `xAI account scope references missing store account`.
  - Remove the dir.
- `xai_oauth_accepts_pool_accounts_and_keeps_the_off_origin_guard`: default config with
  `grok.accounts = vec![account("main")]` validates. Then, with that account still present:
  `base_url = "http://cli-chat-proxy.grok.com/v1"` gives `XaiOauthNotHttps`;
  `"https://evil.example.com/v1"` gives `XaiOauthNonXaiHost`; `kind = ProviderKind::Anthropic` gives
  `XaiOauthWrongKind`.
- `xai_oauth_rejects_invalid_and_duplicate_pool_account_names`: `[account("Bad")]` gives
  `InvalidAccountName`; `[account("a"), account("a")]` gives `DuplicateAccountName`.
- `accounts_require_oauth_provider_error_names_xai_oauth`: the `AccountsRequireOauthProvider`
  Display string contains `"xai_oauth"`.

**Acceptance**

```bash
cargo test --all-features --lib config::tests::xai
cargo test --all-features --lib config::tests::accounts_require_oauth_provider
```

Then run the standard gate.

---

## T4 — `[[upstreams]]` `xai_oauth` account scope

- **depends_on:** T3
- **files:** `src/config/upstreams.rs` (primary), `src/config/upstreams/tests.rs`

**Instructions**

1. Replace `XaiOauth {},` (upstreams.rs:103) with:

   ```rust
   /// xAI subscription OAuth. Same `account`/`accounts` scope as `KimiOauth`,
   /// absorbed by `absorb_oauth_scope`; omitting both scans the whole xAI store
   /// (`~/.shunt/accounts/xai/`), and an empty store falls back to the legacy
   /// `~/.shunt/xai-auth.json` (docs/m18-xai-multi-account.md).
   XaiOauth {
       #[serde(default, skip_serializing_if = "Option::is_none")]
       account: Option<String>,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       accounts: Option<Vec<AccountSelection>>,
   },
   ```

2. Replace the absorb arm (upstreams.rs:153) with:

   ```rust
   Self::Map(AuthMap::XaiOauth { account, accounts }) => {
       absorb_oauth_scope(upstream, AuthMode::XaiOauth, account, accounts, provider)?;
   }
   ```

**Tests to add** (`src/config/upstreams/tests.rs`):

- `xai_oauth_map_scopes_store_references_and_inline_accounts`:
  `auth = { mode = "xai_oauth", accounts = ["primary", { name = "inline", token_env = "XAI_TOKEN" }] }`
  on `name = "grok"`, `provider = "grok"`. Gives `auth == XaiOauth`, `account_scope == ["primary"]`,
  `accounts[0].name == "inline"`.
- `xai_oauth_map_accepts_a_single_account`: `account = "solo"` gives `account_scope == ["solo"]`.
- `xai_oauth_mode_only_map_still_normalizes`: `auth = { mode = "xai_oauth" }` gives `XaiOauth`, with
  empty scope and empty accounts (regression for the old `{}` variant).
- `xai_oauth_map_rejects_account_and_accounts_together`: `UpstreamAuthAccountConflict`.
- `xai_oauth_map_rejects_an_empty_accounts_list`: `EmptyUpstreamAccountList`.
- `load_accepts_an_xai_oauth_upstream_scoped_to_named_accounts`: `TempConfig` + `load` (tests.rs:220-256)
  with

  ```toml
  [server]
  default_provider = "grok"

  [[upstreams]]
  name = "grok"
  provider = "grok"
  auth = { mode = "xai_oauth", accounts = ["primary", "backup"] }
  ```

  The load succeeds (validation passes), `auth == XaiOauth`, and
  `account_scope == ["primary", "backup"]`.

**Acceptance**

```bash
cargo test --all-features --lib config::upstreams
```

Then run the standard gate.

---

## T5 — `classify_xai`

- **depends_on:** T3 (same file; run after it)
- **files:** `src/accounts.rs`

**Instructions.** Review decision D1 (round 2): an inference **403 relays**, as in every other
classifier; only 401 and 402 rotate beyond `classify_codex`. The shared `rotation_reason` stays
**untouched** — it already labels a 402 `membership`. Do not edit `rotation_reason` or its tests.

1. Add right after `classify_codex` (search `pub fn classify_codex`; it ends about 15 lines later):

   ```rust
   /// Classify an xAI subscription OAuth (Grok CLI proxy) upstream response for
   /// account-pool failover. Takes the same `(status, headers)` shape as
   /// [`classify_codex`], which it extends: 401 and 402 rotate too.
   ///
   /// - 401: expiry comes from the access token's JWT `exp`, so a rejected
   ///   bearer was revoked early. Like the Kimi pool, the current request cools
   ///   the account down and rotates instead of a same-account refresh retry;
   ///   the pool also marks the account for a forced refresh on its next use
   ///   (`auth::force_xai_refresh`, see `classify_first_xai`).
   /// - 402: a payment/entitlement refusal is account-specific and persistent,
   ///   the Kimi membership case ([`classify_kimi`]).
   /// - 403 relays, as in every other classifier. xAI's documented tier gate
   ///   surfaces at token refresh, where a failed credential resolution already
   ///   cools the account down and rotates; an inference 403 can be a
   ///   content-policy refusal, and re-sending it to every pool account could
   ///   get all of them flagged.
   ///
   /// Never returns `RefreshRetry` or `PauseSame`.
   pub fn classify_xai(status: StatusCode, headers: &HeaderMap) -> FailoverAction {
       if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::PAYMENT_REQUIRED) {
           return FailoverAction::Rotate;
       }
       classify_codex(status, headers)
   }
   ```

**Tests to add** (accounts.rs `mod tests`, near `rotation_reason_is_low_cardinality_and_distinguishes_quota`;
do not edit that test):

- `classify_xai_rotates_auth_membership_quota_and_server_errors`: 200 → Relay;
  401, 402, 429, 500, 502, 503 → Rotate; 400, **403**, 404, 413 → Relay.
- `classify_xai_never_asks_for_a_refresh_retry_or_a_pause`: for every code `100..=599` that
  `StatusCode::from_u16` accepts, the result is neither `RefreshRetry` nor `PauseSame`.
- `every_classifier_relays_a_403`: `classify`, `classify_codex`, `classify_kimi`,
  `classify_antigravity`, and `classify_xai` all return `Relay` for 403 with empty headers.

**Acceptance**

```bash
cargo test --all-features --lib accounts::tests::classify_xai
cargo test --all-features --lib accounts::tests::every_classifier_relays_a_403
cargo test --all-features --lib accounts::tests::rotation_reason
```

`git diff -- src/accounts.rs` shows no change inside `pub fn rotation_reason`. Then run the
standard gate.

---

## T6 — `resolve_xai_account` (hardened refresh client) and two-account refresh isolation

- **depends_on:** T1
- **files:** `src/auth/mod.rs` (primary), `src/auth/xai/auth.rs` (tests only)

**Instructions**

1. In `src/auth/mod.rs`, add right after `resolve_kimi_account` (search `pub async fn resolve_kimi_account`)
   and before `/// Resolve one ChatGPT (Codex) OAuth account`. Review decision D2: pooled accounts
   refresh through the redirect-hardened client, mirroring `resolve_kimi_account`'s `_client`
   parameter and comment.

   ```rust
   /// Resolve one xAI subscription OAuth account for the account pool. Mirrors
   /// [`resolve_kimi_account`]: `token_env` and an explicit `credentials` path
   /// override the default named-account file (`~/.shunt/accounts/xai/<name>.json`,
   /// see [`xai::store`]). A `token_env` token is used verbatim and never
   /// refreshed. Every other account gets its own [`xai::auth::XaiAuthStore`] over
   /// its own file, so the store's single-flight refresh and atomic `0600`
   /// writeback stay per file and two accounts never share a token.
   pub async fn resolve_xai_account(
       account: &crate::config::AccountConfig,
       // Unlike the legacy `xai_oauth` arm of `resolve_credential`, the store is
       // not handed the caller's proxy client: its refresh POST carries the
       // account's refresh_token, and xAI rotates it on every refresh, so it goes
       // through the redirect-hardened `token_refresh_client()` instead — a
       // 307/308 from the token endpoint must not forward that body to a
       // plaintext or loopback-spoofing host. The legacy arm is unchanged
       // pending a maintainer decision.
       _client: &reqwest::Client,
   ) -> Result<Credential, AdapterError> {
       if let Some(token_env) = account.token_env.as_deref() {
           let access_token = env::var(token_env)
               .ok()
               .filter(|value| !value.is_empty())
               .ok_or_else(|| auth_error(format!("{token_env} is not set")))?;
           return Ok(Credential::XaiOauth { access_token });
       }
       let path = account
           .credentials
           .as_deref()
           .map(PathBuf::from)
           .unwrap_or_else(|| xai::store::account_path(&account.name));
       let store =
           xai::auth::XaiAuthStore::new(path, crate::auth::shared::token_refresh_client());
       store
           .get_valid()
           .await
           .map(|credential| Credential::XaiOauth {
               access_token: credential.access_token,
           })
   }
   ```

2. Do **not** change `resolve_credential`'s `XaiOauth` arm (search `AuthMode::XaiOauth =>` in
   `resolve_credential`).

**Tests to add**

- `src/auth/mod.rs` `mod tests`: add `resolve_xai_account` to the `use super::{…}` list at the top
  of the module. Add a helper:

  ```rust
  /// An unsigned JWT that `is_token_valid_at` accepts until 2096, distinct per `tag`.
  fn xai_token(tag: &str) -> String {
      use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
      let payload = serde_json::json!({ "exp": 4_000_000_000u64, "sub": tag });
      format!(
          "x.{}.y",
          URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
      )
  }
  ```

  Every test below starts with `let _guard = crate::auth::xai::store::TEST_ENV_LOCK.lock().await;`.
  - `resolves_xai_account_token_env_verbatim`:
    `let _env = crate::auth::shared::EnvVarGuard::set("SHUNT_TEST_XAI_POOL_TOKEN", "  xai-token-verbatim  ");`.
    An account with `token_env: Some("SHUNT_TEST_XAI_POOL_TOKEN".into())` resolves to
    `Credential::XaiOauth { access_token: "  xai-token-verbatim  ".into() }`.
  - `xai_account_with_an_unset_token_env_fails_resolution`: `token_env = "SHUNT_TEST_XAI_POOL_TOKEN_UNSET"`
    (never set) gives `Err`.
  - `name_only_xai_account_resolves_the_store_file`: set `SHUNT_XAI_ACCOUNTS_DIR` to a fresh temp
    dir with `EnvVarGuard`, then
    `crate::auth::xai::store::store_oauth_tokens("main", &xai_token("main"), "refresh", None).unwrap();`.
    `AccountConfig { name: "main".into(), ..Default::default() }` resolves to
    `Credential::XaiOauth { access_token: xai_token("main") }`. Remove the dir.
  - `xai_account_credentials_path_overrides_the_store`: same store setup with `main.json`
    (token `xai_token("main")`), plus
    `crate::auth::xai::auth::write_tokens(&dir.join("other.json"), &xai_token("other"), "refresh", None)`.
    An account named `main` with `credentials = <dir>/other.json` resolves to `xai_token("other")`.
  - `missing_xai_account_file_fails_resolution`: `SHUNT_XAI_ACCOUNTS_DIR` points at a fresh,
    never-created dir; a name-only account gives `Err`.
- `src/auth/xai/auth.rs` `mod tests` (helpers `jwt`, `future_exp`, `temp_path` already exist there):

  ```rust
  #[tokio::test]
  async fn concurrent_refreshes_of_two_accounts_write_only_their_own_files() {
      use wiremock::matchers::{body_string_contains, method, path};
      use wiremock::{Mock, MockServer, ResponseTemplate};

      fn jwt_for(sub: &str) -> String {
          let payload = json!({ "exp": future_exp(), "sub": sub });
          format!(
              "x.{}.y",
              URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
          )
      }

      // xAI consumes a refresh token on use: each account's grant must be
      // redeemed exactly once, and each rotated pair must land in its own file.
      let server = MockServer::start().await;
      let access_a = jwt_for("a");
      let access_b = jwt_for("b");
      Mock::given(method("POST"))
          .and(path("/token"))
          .and(body_string_contains("refresh_token=refresh-a"))
          .respond_with(ResponseTemplate::new(200).set_body_json(json!({
              "access_token": access_a,
              "refresh_token": "refresh-a2"
          })))
          .expect(1)
          .mount(&server)
          .await;
      Mock::given(method("POST"))
          .and(path("/token"))
          .and(body_string_contains("refresh_token=refresh-b"))
          .respond_with(ResponseTemplate::new(200).set_body_json(json!({
              "access_token": access_b,
              "refresh_token": "refresh-b2"
          })))
          .expect(1)
          .mount(&server)
          .await;

      let file_a = temp_path("pair-a");
      let file_b = temp_path("pair-b");
      write_tokens(&file_a, &jwt(0), "refresh-a", None).unwrap();
      write_tokens(&file_b, &jwt(0), "refresh-b", None).unwrap();
      let token_url = format!("{}/token", server.uri());
      let store_a =
          XaiAuthStore::with_token_url(file_a.clone(), reqwest::Client::new(), token_url.clone());
      let store_b = XaiAuthStore::with_token_url(file_b.clone(), reqwest::Client::new(), token_url);

      let (a, b) = tokio::join!(store_a.get_valid(), store_b.get_valid());
      assert_eq!(a.unwrap().access_token, access_a);
      assert_eq!(b.unwrap().access_token, access_b);
      let stored_a = parse_tokens(&read_auth_value(&file_a).unwrap()).unwrap();
      let stored_b = parse_tokens(&read_auth_value(&file_b).unwrap()).unwrap();
      assert_eq!(stored_a.refresh_token, "refresh-a2");
      assert_eq!(stored_b.refresh_token, "refresh-b2");
      server.verify().await;
      let _ = std::fs::remove_dir_all(file_a.parent().unwrap());
      let _ = std::fs::remove_dir_all(file_b.parent().unwrap());
  }

  #[tokio::test]
  async fn hardened_refresh_refuses_a_redirect_to_plain_http_and_keeps_the_file() {
      use wiremock::matchers::{method, path};
      use wiremock::{Mock, MockServer, ResponseTemplate};

      // A 307 replays the POST body, which carries the refresh token. The
      // client `resolve_xai_account` hands the store must refuse a hop to a
      // non-loopback `http://` host instead of following it, and the stored
      // pair must stay byte-for-byte as it was.
      let server = MockServer::start().await;
      Mock::given(method("POST"))
          .and(path("/token"))
          .respond_with(
              ResponseTemplate::new(307)
                  .insert_header("location", "http://refresh-thief.example/token"),
          )
          .expect(1)
          .mount(&server)
          .await;

      let file = temp_path("redirect");
      write_tokens(&file, &jwt(0), "old-refresh", None).unwrap();
      let before = std::fs::read(&file).unwrap();
      let store = XaiAuthStore::with_token_url(
          file.clone(),
          crate::auth::shared::token_refresh_client(),
          format!("{}/token", server.uri()),
      );

      let error = store.get_valid().await.unwrap_err();
      assert_eq!(error.message, "authentication failed");
      assert_eq!(std::fs::read(&file).unwrap(), before);
      server.verify().await;
      let _ = std::fs::remove_dir_all(file.parent().unwrap());
  }
  ```

  Limitation to record in the report: `XaiAuthStore::new` hard-codes the production token URL, so
  this test proves the hardened client refuses the hop. It cannot prove that `resolve_xai_account`
  picks that client; the reviewer checks that line by eye.

**Acceptance**

```bash
cargo test --all-features --lib xai_account
cargo test --all-features --lib auth::xai::auth
```

Then run the standard gate.

---

## T7a — Responses pool: per-family resolution, classification, and wording

- **depends_on:** T3, T5, T6
- **files:** `src/adapters/responses/pool.rs` (primary), `src/adapters/responses/pool_xai_tests.rs` (new),
  `src/accounts.rs` (one comment line)

**Instructions.** Line numbers are HEAD's (e9b85be); find each spot by the quoted text. Edit
bottom-up so earlier positions stay valid.

1. Next to `exhausted_error` (pool.rs:1423), add:

   ```rust
   /// The pooled provider's auth mode — `chatgpt_oauth`, or `xai_oauth` since the
   /// xAI pool rides this loop (docs/m18-xai-multi-account.md) — read the way
   /// `forward` reads it. It only picks gateway-owned error wording
   /// ([`mapped_upstream_error`], [`no_upstream_response_message`]); credential
   /// resolution and classification key on each account's `store_family`.
   fn provider_auth(state: &AppState, route: &Route) -> AuthMode {
       state
           .config
           .provider(&route.provider)
           .map(|provider| provider.auth)
           .unwrap_or(AuthMode::ChatgptOauth)
   }

   /// The gateway-owned message when every pool account failed before any
   /// upstream response, named for the pool's family.
   pub(super) fn no_upstream_response_message(auth: AuthMode) -> &'static str {
       match auth {
           AuthMode::XaiOauth => {
               "all xAI OAuth accounts failed before receiving an upstream response"
           }
           _ => "all Codex OAuth accounts failed before receiving an upstream response",
       }
   }
   ```

2. Replace the hard-coded wording and auth:
   - `exhausted_error` (1434-1436): `transport_error(no_upstream_response_message(auth).to_string())`.
   - `forward_chatgpt_oauth` (1074, `let auth = AuthMode::ChatgptOauth;`): `let auth = provider_auth(&state, &route);`.
   - `forward_chatgpt_oauth` (1033, `auth: AuthMode::ChatgptOauth,` in `PoolStreamContext`):
     `auth: provider_auth(&state, &route),`.
   - `pool_events_stream` (596-597, the string inside `transport_error(…)`):
     `no_upstream_response_message(auth).to_string()`.
   - `pool_or_single_events` (353, `auth: AuthMode::ChatgptOauth,`): `auth: single.auth,`.
   - `forward_chatgpt_oauth_stream` (210, `auth: AuthMode::ChatgptOauth,` in `HttpSendContext`):
     `auth: provider_auth(&state, &route),`.
3. `resolve_or_cooldown` (1574): make this the first statement of the body

   ```rust
   if account.store_family == Some(accounts::StoreFamily::Xai) {
       return resolve_xai_or_cooldown(state, route, account).await;
   }
   ```

   and add right after `resolve_or_cooldown`:

   ```rust
   /// [`resolve_or_cooldown`] for an xAI pool account (`StoreFamily::Xai`):
   /// resolve through [`auth::resolve_xai_account`], and on failure cool the
   /// account down for 5 minutes and rotate — the Kimi pool's treatment
   /// (`adapters::anthropic::forward_kimi_oauth`). The xAI store reports no
   /// terminal/transient split, so no `needs_relogin` mark is set.
   async fn resolve_xai_or_cooldown(
       state: &AppState,
       route: &Route,
       account: &AccountConfig,
   ) -> Option<Credential> {
       match auth::resolve_xai_account(account, &state.http_client).await {
           Ok(credential) => Some(credential),
           Err(error) => {
               state.accounts.cooldown(
                   &route.provider,
                   account,
                   Duration::from_secs(5 * 60),
                   "auth",
               );
               tracing::warn!(
                   provider = %route.provider,
                   account = %account.name,
                   error = %error.message,
                   "failed to resolve xAI OAuth account"
               );
               None
           }
       }
   }
   ```

4. `classify_first` (1885): make this the first statement of the body

   ```rust
   if account.store_family == Some(accounts::StoreFamily::Xai) {
       return Ok(classify_first_xai(state, route, account, upstream));
   }
   ```

   and add right after `classify_first` (D1, round 2: `classify_xai` relays a 403, so no label
   logic is needed here; the shared `rotation_reason` already labels a 402 `membership`):

   ```rust
   /// [`classify_first`] for an xAI pool account: the same cooldown-and-rotate
   /// bookkeeping, classified with [`accounts::classify_xai`] and without the
   /// Codex-only 400 model-refusal read, so a relayed 4xx keeps its body unread.
   /// `classify_xai` never asks for a refresh retry; the defensive arms rotate.
   fn classify_first_xai(
       state: &AppState,
       route: &Route,
       account: &AccountConfig,
       upstream: reqwest::Response,
   ) -> FirstOutcome {
       let status = upstream.status();
       match accounts::classify_xai(status, upstream.headers()) {
           FailoverAction::Relay => FirstOutcome::Relay(upstream),
           FailoverAction::Rotate | FailoverAction::RefreshRetry | FailoverAction::PauseSame => {
               if status == reqwest::StatusCode::UNAUTHORIZED {
                   // A file-backed OAuth token is only sent while unexpired, so a
                   // 401 means xAI revoked it early: refresh on this account's
                   // next use rather than resend it every cooldown until its
                   // `exp`. `force_xai_refresh` does nothing for a static
                   // `token_env` bearer, which is never refreshed.
                   auth::force_xai_refresh(account);
               }
               let cooldown = rotate_cooldown(status, upstream.headers());
               state.accounts.cooldown(
                   &route.provider,
                   account,
                   cooldown,
                   accounts::rotation_reason(status, upstream.headers()),
               );
               tracing::warn!(
                   provider = %route.provider,
                   account = %account.name,
                   status = %status,
                   "xAI pool account failed over; cooling down and rotating to the next account"
               );
               FirstOutcome::Rotate(upstream)
           }
       }
   }
   ```

5. Module doc (pool.rs:1-3): add a sentence saying the same loop serves the xAI pool
   (`StoreFamily::Xai`, docs/m18), with per-family resolution and classification.
6. `src/accounts.rs`, `fn account_key`: inside the `unwrap_or_else` fallback comment
   (around "Only a caller that bypassed `resolve_pool_accounts`…"), add one sentence:
   `// An unstamped account on the built-in xAI \`grok\` provider lands on Claude here; every xAI caller goes through \`resolve_pool_accounts\`, which stamps \`StoreFamily::Xai\`.`
   This is a comment only; no code change.
7. At the very end of pool.rs (after the existing `mod tests`), add:

   ```rust
   #[cfg(test)]
   #[path = "pool_xai_tests.rs"]
   mod xai_tests;
   ```

**Tests** — create `src/adapters/responses/pool_xai_tests.rs` with exactly this scaffold. Its helpers
are the bodies of `pool.rs`'s test helpers (`pool_route` ~2024, `pool_state` ~2559, `pool_turn`
~2576, `COMPLETED_TURN` ~2748), with only the provider/model changed. T7a uses every helper here;
T7b adds two more.

```rust
//! Module-level tests for the xAI account pool (`StoreFamily::Xai`) riding the
//! Codex Responses pool loop (docs/m18-xai-multi-account.md).
//!
//! The `xai_oauth` bearer guard (`Config::validate`) pins an xAI provider to an
//! https x.ai/grok.com host with no loopback exception, so these tests route to
//! the loopback `openai` provider and stamp each account `StoreFamily::Xai`.
//! Credential resolution and failover classification key on that stamp, so
//! they are exercised as in production.
//!
//! Fidelity gap: the route's provider auth here is `api_key`, so
//! `provider_auth` yields `ApiKey` and the gateway-owned error wording these
//! pool runs produce is the generic mapping, not the xAI one. The xAI wording
//! is asserted elsewhere: `provider_auth` on the built-in `grok` provider and
//! `exhausted_error` with `AuthMode::XaiOauth` (below), `mapped_upstream_error`
//! with `XaiOauth` in `error.rs`'s tests, and the gateway path in
//! `tests/xai_multi_account.rs`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::json;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::context::{PoolForward, TurnOptions};
use super::{exhausted_error, forward_chatgpt_oauth, no_upstream_response_message, provider_auth};
use crate::{
    accounts::StoreFamily,
    config::{AccountConfig, AuthMode, Config},
    routing::{AdapterKind, Route},
    server::AppState,
};

// `COMPLETED_TURN` from `pool.rs`'s tests.
const COMPLETED_TURN: &str = "event: response.created\ndata: {\"response\":{\"id\":\"resp_2\"}}\n\n\
     event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n\
     event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "shunt-xai-pool-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// An unsigned JWT that `is_token_valid_at` accepts until 2096, distinct per `tag`.
fn xai_token(tag: &str) -> String {
    let payload = json!({ "exp": 4_000_000_000u64, "sub": tag });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

/// A `StoreFamily::Xai` account backed by its own credentials file holding a
/// valid token, so resolution needs no env var and no refresh.
fn xai_account(dir: &Path, name: &str) -> (AccountConfig, String) {
    let token = xai_token(name);
    let path = dir.join(format!("{name}.json"));
    crate::auth::xai::auth::write_tokens(&path, &token, "refresh", None).unwrap();
    let account = AccountConfig {
        name: name.to_string(),
        credentials: Some(path.to_string_lossy().into_owned()),
        store_family: Some(StoreFamily::Xai),
        ..Default::default()
    };
    (account, token)
}

// `pool_state` from `pool.rs`'s tests, pointed at the `openai` provider.
fn xai_state(base_url: String) -> AppState {
    let mut config = Config::default();
    config.providers.get_mut("openai").unwrap().base_url = base_url;
    AppState::new(config, reqwest::Client::new()).unwrap()
}

// `pool_route` from `pool.rs`'s tests, with the `openai` provider and a Grok slug.
fn xai_route() -> Route {
    Route {
        provider: "openai".to_string(),
        adapter: AdapterKind::Responses,
        model: "grok-4.6".to_string(),
        upstream_model: "grok-4.6".to_string(),
        effort: None,
        service_tier: None,
    }
}

// `pool_turn` from `pool.rs`'s tests.
fn xai_turn(accounts: Vec<AccountConfig>, stream: bool) -> PoolForward {
    PoolForward {
        pool_key: None,
        session_id: None,
        compact: crate::request::CompactionMark::default(),
        delegation: None,
        upstream_body: std::sync::Arc::new(json!({"input": []})),
        accounts_config: accounts,
        turn: TurnOptions {
            client_wants_stream: stream,
            thinking_enabled: false,
            tool_search_native: false,
            stop_sequences: Vec::new(),
            response_bounds: crate::adapters::ResponseBounds::default(),
        },
        estimate_input: None,
    }
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body is readable");
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn no_upstream_response_message_names_the_pool_family() {
    assert_eq!(
        no_upstream_response_message(AuthMode::XaiOauth),
        "all xAI OAuth accounts failed before receiving an upstream response"
    );
    assert_eq!(
        no_upstream_response_message(AuthMode::ChatgptOauth),
        "all Codex OAuth accounts failed before receiving an upstream response"
    );
}

#[tokio::test]
async fn provider_auth_reads_xai_oauth_for_the_builtin_grok_provider() {
    let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();
    let mut grok = xai_route();
    grok.provider = "grok".to_string();
    assert_eq!(provider_auth(&state, &grok), AuthMode::XaiOauth);
    assert_eq!(provider_auth(&state, &xai_route()), AuthMode::ApiKey);
}

#[tokio::test]
async fn xai_exhaustion_without_a_response_uses_the_xai_wording() {
    let error = exhausted_error(None, AuthMode::XaiOauth, None).await;
    let body = body_text(*error.response).await;
    assert!(
        body.contains("all xAI OAuth accounts failed before receiving an upstream response"),
        "{body}"
    );
}

#[tokio::test]
async fn xai_exhaustion_on_a_403_keeps_the_tier_gate_hint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;
    let upstream = reqwest::Client::new()
        .post(server.uri())
        .send()
        .await
        .expect("mock answers");
    let error = exhausted_error(Some(upstream), AuthMode::XaiOauth, None).await;
    assert_eq!(error.response.status(), StatusCode::FORBIDDEN);
    let body = body_text(*error.response).await;
    assert!(body.contains("XAI_API_KEY"), "{body}");
}

#[tokio::test]
async fn xai_pool_sends_the_selected_accounts_bearer_with_grok_cli_headers() {
    let dir = temp_dir("bearer");
    let (account, token) = xai_account(&dir, "xai-a");
    let server = MockServer::start().await;
    let authorization = format!("Bearer {token}");
    Mock::given(method("POST"))
        .and(header("authorization", authorization.as_str()))
        .and(header("x-xai-token-auth", "xai-grok-cli"))
        .respond_with(ResponseTemplate::new(200).set_body_string(COMPLETED_TURN))
        .mount(&server)
        .await;
    let state = xai_state(server.uri());
    let (status, response) =
        forward_chatgpt_oauth(state, xai_route(), xai_turn(vec![account], false))
            .await
            .expect("the account serves the turn");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response.headers()["x-shunt-account"], "xai-a");
    let _ = std::fs::remove_dir_all(dir);
}
```

If `error.response` is a `Box<Response>`, `*error.response` moves it out; if the compiler disagrees,
adapt the dereference only. If an unmatched header makes the mock answer 404, the bearer test fails
on `expect` — that is the intended signal.

**Acceptance**

```bash
cargo test --all-features --lib adapters::responses::pool
cargo test --all-features --test codex_multi_account --test inbound_codex_endpoint
```

Every existing `adapters::responses::pool::tests` test passes unchanged. They all route to `codex`,
so `provider_auth` yields `ChatgptOauth`, as the old hard-code did. Then run the standard gate.

---

## T7b — Responses pool: xAI rotation and 403-relay tests

- **depends_on:** T7a
- **files:** `src/adapters/responses/pool_xai_tests.rs`

**Instructions.** Tests only; no production change. If a test fails, stop and report — do not edit
`pool.rs`. Add these two helpers below `xai_turn`:

```rust
/// A `StoreFamily::Xai` account whose credentials file does not exist.
fn unresolvable_xai_account(dir: &Path, name: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        credentials: Some(dir.join("absent.json").to_string_lossy().into_owned()),
        store_family: Some(StoreFamily::Xai),
        ..Default::default()
    }
}

/// Answer `response` only to requests carrying `token` as their bearer, so
/// each pool account gets its own scripted upstream.
async fn mount_for_token(server: &MockServer, token: &str, response: ResponseTemplate) {
    let authorization = format!("Bearer {token}");
    Mock::given(method("POST"))
        .and(header("authorization", authorization.as_str()))
        .respond_with(response)
        .mount(server)
        .await;
}
```

**Tests to add.** In each test, a fresh `AppState` round-robins from index 0, so account `a`
(first in the vec) is tried first.

```rust
#[tokio::test]
async fn xai_pool_relays_a_403_from_the_first_account_without_rotating_or_cooling() {
    // D1 (round 2): an inference 403 may be a content-policy refusal, so it is
    // relayed from the first account — never re-sent to the rest of the pool —
    // and the account is not cooled down.
    let dir = temp_dir("relay-403");
    let (a, token_a) = xai_account(&dir, "xai-a");
    let (b, token_b) = xai_account(&dir, "xai-b");
    let server = MockServer::start().await;
    mount_for_token(
        &server,
        &token_a,
        ResponseTemplate::new(403).set_body_string("forbidden"),
    )
    .await;
    mount_for_token(
        &server,
        &token_b,
        ResponseTemplate::new(200).set_body_string(COMPLETED_TURN),
    )
    .await;
    let state = xai_state(server.uri());
    let error = forward_chatgpt_oauth(
        state.clone(),
        xai_route(),
        xai_turn(vec![a.clone(), b], false),
    )
    .await
    .expect_err("a 403 is relayed, not rotated past");
    assert_eq!(error.response.status(), StatusCode::FORBIDDEN);
    let requests = server.received_requests().await.expect("mock records");
    assert_eq!(requests.len(), 1, "the second account is never called");
    let expected = format!("Bearer {token_a}");
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some(expected.as_str()),
        "the one request went to the first account"
    );
    let snapshot = state
        .accounts
        .snapshot("openai", std::slice::from_ref(&a), None, None);
    assert_eq!(
        snapshot[0].cooldown_secs_remaining, None,
        "a relayed 403 sets no cooldown"
    );
    let _ = std::fs::remove_dir_all(dir);
}
```

- `xai_pool_rotates_off_a_401_without_a_same_account_refresh_retry`: `a` → 401, `b` → 200
  (the rotation shape: `forward_chatgpt_oauth(…).await.expect(…)` returns Ok(200)).
  Assert exactly 2 requests (no retry on `a`), `x-shunt-account == "xai-b"`, `a` cooled, and
  `auth::xai::auth::force_refresh_pending(&a_file)` is true (the 401 marked `a`'s credential file, so
  its next use refreshes before sending).
- `xai_pool_cools_an_unresolvable_account_for_five_minutes_and_rotates`:
  `a = unresolvable_xai_account(&dir, "xai-a")`, `b` → 200. Assert exactly **1** request,
  `x-shunt-account == "xai-b"`, and `a`'s `cooldown_secs_remaining` is `Some(secs)` with `secs > 60`.

**Acceptance**

```bash
cargo test --all-features --lib adapters::responses::pool::xai_tests
```

Then run the standard gate.

---

## T8 — Responses pool: remaining xAI classification tests

- **depends_on:** T7b
- **files:** `src/adapters/responses/pool_xai_tests.rs`

**Instructions.** Tests only, reusing the T7a/T7b helpers (`mount_for_token`, `xai_account`,
`body_text`, …). No production change. If a test fails, stop and report; do not change pool.rs.

**Tests to add**

- `xai_pool_429_cools_the_account_for_its_retry_after`: `a` → `ResponseTemplate::new(429).insert_header("retry-after", "120")`,
  `b` → 200. `a`'s `cooldown_secs_remaining` is in `61..=120`.
- `xai_pool_rotates_off_a_5xx`: `a` → 503, `b` → 200. 2 requests, and `x-shunt-account == "xai-b"`.
- `xai_pool_relays_a_400_unread_and_never_as_a_model_refusal`: `a` → `400` with JSON
  `{"detail": "The 'grok-4.6' model is not supported when using Codex with a ChatGPT account."}`,
  `b` → 200. The non-streaming call is `Err` with `error.response.status() == 400`; 1 request; `a`'s
  `cooldown_secs_remaining == None`; and `body_text(*error.response)` contains `model is not supported`.
- `xai_pool_exhausted_by_402s_relays_the_last_one`: `a` and `b` → 402. The result is `Err` after
  2 requests, with `error.response.status() == StatusCode::BAD_GATEWAY` (402 is outside the
  client-facing status table in `src/model/responses.rs`, `mapped_error`, so it surfaces as 502).
- `xai_pool_streaming_arm_rotates_and_attributes_the_winner`: `xai_turn(…, true)`, `a` → 402,
  `b` → `200 COMPLETED_TURN`. The body (`body_text(response)`) has exactly one
  `event: message_start`, contains `event: account`, contains `xai-b`, and contains `"text":"hi"`.
  Pattern: `pool_streaming_arm_rotates_and_relays_from_the_second_account` in pool.rs's tests.

**Acceptance**

```bash
cargo test --all-features --lib adapters::responses::pool::xai_tests
```

Then run the standard gate.

---

## T9 — Dispatch gates and gateway tests (legacy-file rule)

- **depends_on:** T7a
- **files:** `src/adapters/responses/mod.rs` (primary), `tests/xai_multi_account.rs` (new)

**Step 0 — characterization first.** Create `tests/xai_multi_account.rs` with the helpers and the
two `legacy_*` tests below, then run them **before** touching `mod.rs`:
`cargo test --all-features --test xai_multi_account legacy`. Both must pass on the unmodified
dispatch, because they pin today's behavior. Both assert the exact text
`xAI auth not found; run shunt login xai`:

- Before the change, a streaming turn defers `resolve_credential` inside `forward_http`.
- After the change, the same deferred resolver runs inside `pool_or_single_events`.

The text is the same on both sides. If either test fails on the unmodified tree, stop and report —
do not adjust the expected strings.

**Instructions** (`src/adapters/responses/mod.rs`; line numbers are HEAD's)

1. `:302` `if auth == AuthMode::ChatgptOauth {` →
   `if matches!(auth, AuthMode::ChatgptOauth | AuthMode::XaiOauth) {`. Extend the comment above
   (296-300) to say the xAI pool (docs/m18) uses the same branch.
2. `:313-326`: replace
   `let accounts_config = …; let account_scope = …; let scan = Box::pin(async move { auth::shared::resolve_pool_accounts("codex", …).await });`
   with:

   ```rust
   let scan_provider = provider.clone();
   let scan = Box::pin(async move { scan_provider.resolve_pool_accounts().await });
   ```

3. `:359-367`: `let accounts = provider.resolve_pool_accounts().await.map_err(own_error)?;`
4. `:588`: same `matches!` as step 1.
5. `:593-602`: `let accounts = match provider.resolve_pool_accounts().await {` (arms unchanged).
6. `:646`: `auth: AuthMode::ChatgptOauth,` → `auth,`.
7. `:755-757`: `adapter_error_envelope(transport_error(no_upstream_response_message(auth).to_string()))`.
   Add `no_upstream_response_message` to the `use self::pool::{…}` list (57-60).
8. `chain_attempt` doc comment (~505-511): "A pooled `chatgpt_oauth` or `xai_oauth` route".
9. If clippy reports `auth` (the module) as unused in
   `use crate::{ auth::{self, resolve_credential, Credential}, … }` (line 32), drop `self` from it.

**Tests** — `tests/xai_multi_account.rs`. Start with a `//!` comment: the xai_oauth guard pins the
provider to an https grok.com host, so these tests drive only outcomes that never leave the
process, and pool rotation is covered in `src/adapters/responses/pool_xai_tests.rs`. Helpers:

- `mod common;`
- `TestGateway` with `Drop` and a `start_gateway(config)`, copied from
  `tests/kimi_multi_account.rs:36-98` (without `state`).
- `can_bind_loopback()`, copied from `tests/kimi_multi_account.rs:100-112`.
- `temp_root(tag)` (pid + nanos).
- `grok_config()`: `Config::default()` plus
  `RouteConfig { model: "grok-pooled", provider: "grok", upstream_model: Some("grok-4.6"), effort: None, service_tier: None }`.
- `post_messages(&gateway, stream: bool) -> (StatusCode, String)`: POST
  `{"model":"grok-pooled","max_tokens":16,"stream":<stream>,"messages":[{"role":"user","content":"hi"}]}`
  and return the status plus the full body text.

Every test does `if !can_bind_loopback() { return; }`, then `let mut vars = common::env_lock().await;`,
`vars.set("SHUNT_XAI_ACCOUNTS_DIR", root.join("accounts"))`, and
`vars.set("SHUNT_XAI_AUTH_FILE", root.join("xai-auth.json"))`. Never create `xai-auth.json`. Remove
`root` at the end.

1. `legacy_auth_file_answers_non_streaming_turns_when_the_xai_store_is_empty` (step 0): no
   `accounts` dir. Status is 401 and the body contains `xAI auth not found; run shunt login xai`.
2. `legacy_auth_file_answers_streaming_turns_when_the_xai_store_is_empty` (step 0): same setup with
   `stream: true`. The body contains `xAI auth not found; run shunt login xai`.
3. `named_xai_accounts_replace_the_legacy_auth_file`: create `accounts/a.json` with content `{}`.
   Non-streaming: status 502, the body contains `all upstreams failed`, and it does **not** contain
   `xAI auth not found`.
4. `named_xai_accounts_streaming_exhaustion_names_the_xai_pool`: same files, `stream: true`. The body
   contains `all xAI OAuth accounts failed before receiving an upstream response`.
5. `an_xai_account_scope_naming_a_missing_account_is_a_gateway_error`: `accounts/real.json` = `{}`,
   and `config.providers.get_mut("grok").unwrap().account_scope = vec!["ghost".to_string()]`.
   Non-streaming: status 502, and the body contains
   `xAI account scope references missing store account` and `ghost`.

**Acceptance**

```bash
cargo test --all-features --test xai_multi_account
cargo test --all-features --test codex_multi_account --test responses_chain_stream --test failover --test env_lock_coverage
```

Then run the standard gate.

---

## T9b — Routed inbound Codex endpoint: pick one xAI pool account

- **depends_on:** T6, T7a
- **files:** `src/adapters/responses/inbound_routed.rs`

**Why.** `forward_codex_routed` resolves only the legacy file (`inbound_routed.rs:62` →
`resolve_credential`). Without this ticket, the `mv` migration that T2 and T12 recommend breaks every
`[[server.codex_endpoint.routes]]` → `xai_oauth` route (review decision D3).

**Instructions**

1. Imports: add `use crate::config::AuthMode;` and `use super::error::own_error;`. `own_error` is
   `pub(super)` in `responses::error`, so it is visible here.
2. In `forward_codex_routed`, replace
   `let credential = resolve_credential(&state.config, &route, &state.http_client).await?;` with:

   ```rust
   let credential =
       match routed_xai_pool_credential(&state, &route, session_id.as_deref()).await? {
           Some(credential) => credential,
           None => resolve_credential(&state.config, &route, &state.http_client).await?,
       };
   ```

3. Add below `forward_codex_routed`:

   ```rust
   /// The credential for a route to an `xai_oauth` provider whose account pool is
   /// non-empty: the first account in pool selection order, which honors the
   /// provider's configured `account`/`accounts` scope, `priority`, `disabled`,
   /// and the admin pause. `None` for any other provider, or for an empty xAI
   /// pool, where `resolve_credential`'s single-account file applies — the same
   /// rule as the outbound pool (docs/m18-xai-multi-account.md), mirroring
   /// `adapters::gemini::forward`'s empty-pool fallback. One account and one send:
   /// this path still never rotates or cools an account down.
   async fn routed_xai_pool_credential(
       state: &AppState,
       route: &Route,
       session_id: Option<&str>,
   ) -> Result<Option<Credential>, AdapterError> {
       let Some(provider) = state.config.provider(&route.provider) else {
           return Ok(None);
       };
       if provider.auth != AuthMode::XaiOauth {
           return Ok(None);
       }
       let accounts = provider.resolve_pool_accounts().await.map_err(own_error)?;
       if accounts.is_empty() {
           return Ok(None);
       }
       let (order, _cap) = state.accounts.select_order_with_cap(
           &route.provider,
           &accounts,
           session_id,
           Some(route.upstream_model.as_str()),
           state.config.server.pool.as_ref(),
       );
       let Some(&index) = order.first() else {
           return Err(own_error(format!(
               "provider '{}' has {} account(s) but none are selectable",
               route.provider,
               accounts.len()
           )));
       };
       crate::auth::resolve_xai_account(&accounts[index], &state.http_client)
           .await
           .map(Some)
   }
   ```

4. Module doc (lines 1-26): change "There is no pool and no failover here: one credential, one
   attempt" so it says that an `xai_oauth` route with named accounts takes that one credential from
   the first account in pool selection order, and there is still no failover.

Gateway errors here keep the OpenAI Responses shape: `src/codex_endpoint.rs:122-126` re-shapes every
`AdapterError` this function returns.

**Tests to add** (inbound_routed.rs `mod tests`, after `an_xai_oauth_route_carries_the_grok_cli_identity`;
reuse its config shape — the `openai` provider re-pointed to `https://cli-chat-proxy.grok.com/v1`
with `auth = XaiOauth` — and the existing `route("openai")` helper). Every test is `#[tokio::test]`
and starts with `let _guard = crate::auth::xai::store::TEST_ENV_LOCK.lock().await;`. Store files
come from `crate::auth::xai::store::store_oauth_tokens(name, &token, "refresh", None)` with a
far-future JWT helper like T6's `xai_token`, under
`EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir)`. Nothing is sent: these call
`routed_xai_pool_credential` directly.

- `an_xai_route_with_named_accounts_uses_the_first_selected_account`: store `a` and `b`. With no
  scope, the result is `Some(Credential::XaiOauth { access_token: token_a })`. With
  `provider.account_scope = vec!["b".into()]` (on a fresh state), the result is `token_b`.
- `an_xai_route_with_an_empty_store_falls_back_to_the_single_account_file`: dir never created →
  `Ok(None)`.
- `a_non_xai_route_never_consults_the_xai_store`: the default `openai` provider (`api_key`) with
  store files present → `Ok(None)`.
- `an_xai_route_whose_accounts_are_all_disabled_is_a_gateway_error`:
  `provider.accounts = vec![AccountConfig { name: "off".into(), disabled: true, ..Default::default() }]`
  → `Err` with `error.response.status() == StatusCode::BAD_GATEWAY`.

**Acceptance**

```bash
cargo test --all-features --lib adapters::responses::inbound_routed
cargo test --all-features --test inbound_codex_routes --test inbound_codex_endpoint
```

Then run the standard gate.

---

## T10a — Store isolation for existing fixtures

- **depends_on:** T3
- **files:** `tests/common/mod.rs` (primary), `src/usage/tests.rs`; plus isolation-only edits that
  step 3 may require in other `tests/*.rs` files or in `src/admin/mod.rs`'s test module

**Why (review decision D6).** Once T10b lands, every pool listing scans the built-in `grok`
provider's store, which defaults to the operator's real `~/.shunt/accounts/xai`. This ticket lands
first, so no existing test ever reads it. These edits are **isolation only**: no assertion,
expectation, or config change in any existing test. No production code changes.

**Instructions**

1. `tests/common/mod.rs`: add the helper below, and in both `set_env` and `set_env_blocking`
   call `guard.set("SHUNT_XAI_ACCOUNTS_DIR", isolated_xai_store());` right after `guard` is built,
   before the caller's `vars` loop. A test that needs a populated store still overrides it with
   `vars.set`. Do **not** add a raw environment write, and do not spell the raw std env function
   names in the new comment: `tests/env_lock_coverage.rs` counts them in this file.

   ```rust
   /// A per-process path that is never created. The built-in `grok` provider is
   /// an `xai_oauth` account pool, so anything that resolves pools
   /// (`/admin/api/pool`, `/usage`, an xAI route) scans its store; every guard
   /// points that scan here so no test reads the operator's real
   /// `~/.shunt/accounts/xai`. A test that needs a populated store overrides it
   /// with [`EnvVars::set`].
   fn isolated_xai_store() -> std::path::PathBuf {
       std::env::temp_dir().join(format!(
           "shunt-test-xai-store-{}-absent",
           std::process::id()
       ))
   }
   ```

2. `src/usage/tests.rs`: add this helper. Make
   `let _xai = isolate_xai_store().await;` the first line of every test that calls `get(` (11
   today). T10b's new usage test uses it too.

   ```rust
   /// The built-in `grok` provider is an `xai_oauth` pool, so `get` scans its
   /// account store. Point that scan at a path that never exists, under the lock
   /// the xAI store tests share, so no test reads the operator's real
   /// `~/.shunt/accounts/xai`. Fields drop in declaration order: the variable is
   /// removed before the lock is released.
   struct XaiStoreIsolation {
       _env: crate::auth::shared::EnvVarGuard,
       _lock: tokio::sync::MutexGuard<'static, ()>,
   }

   async fn isolate_xai_store() -> XaiStoreIsolation {
       let lock = crate::auth::xai::store::TEST_ENV_LOCK.lock().await;
       let env = crate::auth::shared::EnvVarGuard::set(
           "SHUNT_XAI_ACCOUNTS_DIR",
           std::env::temp_dir().join(format!(
               "shunt-usage-test-xai-store-{}-absent",
               std::process::id()
           )),
       );
       XaiStoreIsolation { _env: env, _lock: lock }
   }
   ```

3. Run `grep -n "admin/api/pool\|\"/usage\"" tests/*.rs`. Any such test that does not already
   hold `common::env_lock()` / `common::set_env(…)` gets `let _env = common::env_lock().await;` as
   its first line. Then `grep -n "mod tests" src/admin/mod.rs` and check whether any unit test
   there calls the `pool` handler; if one does, give it the step-2 helper pattern (own copy, same
   lock). Report every file this step touched.

**Tests to add:** none. The existing suite is the test: it must stay green with identical
assertions.

**Acceptance**

```bash
cargo test --all-features --test env_guard --test env_lock_coverage --test admin_surface --test pool_pause_api
cargo test --all-features --lib usage::tests
```

`git diff` of the existing tests shows only added isolation lines (no changed assertion). Then run
the standard gate.

---

## T10b — Admin pool, PATCH, and `/usage`

- **depends_on:** T10a
- **files:** `src/admin/mod.rs` (primary), `src/usage.rs`, `tests/admin_surface.rs`, `tests/pool_pause_api.rs`,
  `src/usage/tests.rs`

**Instructions**

1. `src/admin/mod.rs`, `pool()`:
   - Add `| AuthMode::XaiOauth` to the filter `matches!` (1179-1186).
   - In the `match provider.auth` (1187-1233), add before `_ => unreachable!(…)`:

     ```rust
     // xAI resolves through the shared per-family helper rather than another
     // copy of the call above.
     AuthMode::XaiOauth => provider.resolve_pool_accounts().await,
     ```

2. `src/admin/mod.rs`, `patch_pool_account()` (match at 1390-1436): add
   `AuthMode::XaiOauth => provider_cfg.resolve_pool_accounts().await,` before the
   `_ => return bad_request(…)` arm.
3. `src/usage.rs` (449-489): add `| AuthMode::XaiOauth` to the filter, and
   `AuthMode::XaiOauth => provider.resolve_pool_accounts().await,` before `_ => unreachable!(…)`.
4. Do not touch `clear_inert_caps` (`src/admin/mod.rs:1151-1161`): xAI caps stay inert, like Kimi's.

**Tests to add**

- `tests/admin_surface.rs`: `admin_pool_lists_xai_oauth_accounts_read_only`, after
  `admin_pool_lists_kimi_oauth_accounts_read_only` (1173-1210), with the same structure.
  - Use `admin_config("SHUNT_TEST_ADMIN_XAI_POOL")` and
    `vars.set("SHUNT_TEST_ADMIN_XAI_POOL", "ops:xai-pool-secret")`.
  - Set `config.providers.get_mut("grok").unwrap().accounts =
    vec![AccountConfig { name: "xai-primary", token_env: Some("SHUNT_TEST_ADMIN_XAI_TOKEN"), .. }]`
    and `vars.set("SHUNT_TEST_ADMIN_XAI_TOKEN", "xai-access-token")`.
  - In the GET `/admin/api/pool` response, the section with `provider == "grok"` has
    `auth == "xai_oauth"` and `accounts[0].name == "xai-primary"`.
- `tests/pool_pause_api.rs`: `patch_pool_account_pauses_an_xai_oauth_account`.
  - Take `admin_config()` and give `grok.accounts` one entry
    `{ name: "grok-pause-me", credentials: Some(nonexistent_credentials_path()) }`.
  - Set `vars.set("SHUNT_TEST_ADMIN_TOKENS_POOL_PAUSE", "ops:unused")`, call `start`, and get
    `pool_account_ref(&client, &gateway, "grok", "grok-pause-me")`.
  - `PATCH /admin/api/pool/grok/accounts/{ref}` with `{"paused":true}` (write key) returns 200, and
    `pool_accounts(&client, &gateway, "grok")` shows that account with `paused == true`.
- `src/usage/tests.rs`: `aggregate_covers_an_xai_oauth_pool_alongside_codex`, mirroring
  `aggregate_covers_a_kimi_oauth_pool_alongside_claude_and_codex` (696-781).
  - Instead of inserting a provider, set `config.providers.get_mut("grok").unwrap().accounts = vec![xai_account]`.
  - Seed the codex account at 0.25 and the xai account at 0.10, using `StoreFamily::Xai` for the
    latter.
  - Expect `5h.remaining == 0.825`. Before step 3 this test gets 0.75, which is how it pins the change.

The new `src/usage/tests.rs` test starts with `let _xai = isolate_xai_store().await;` (the helper
from T10a).

**Acceptance**

```bash
cargo test --all-features --test admin_surface
cargo test --all-features --test pool_pause_api
cargo test --all-features --lib usage::tests
```

Then run the standard gate.

---

## T11 — Docs: `docs/`

- **depends_on:** T2, T4, T9, T9b, T10b
- **files:** `docs/m18-xai-multi-account.md` (new, primary), `docs/m6-xai-provider.md`, `docs/running.md`,
  `docs/upstreams-failover.md`, `docs/m11-inbound-codex-endpoint.md`

**Instructions.** English only. State facts from PLAN.md and the merged code; invent nothing.

1. `docs/m18-xai-multi-account.md`: a spec in the style of `docs/m15-kimi-oauth.md` and
   `docs/m10-codex-multi-account.md`, with these sections, in order:
   1. *Scope*: pool `xai_oauth` accounts with rotation and failover. No new config keys; one new env
      var `SHUNT_XAI_ACCOUNTS_DIR`. Companion to m6, m10, m15.
   2. *Account store*: path, override, the JSON shape (show the m6 §3 block), `0700`/`0600` atomic
      writes, filename identity, `shunt login xai --name <name>`, `--mode`/`--long-lived` rejected,
      and reusing a name replaces the file.
   3. *Legacy file rule*: the exact rule from PLAN §1, the precedents (Codex M10, Antigravity), the
      `mv` migration with shunt stopped, the login notice, and never copying (refresh-token rotation).
   4. *Configuration*: store scan (no config); `[[providers.grok.accounts]]` with `name`,
      `credentials`, `token_env` (used verbatim, never refreshed), `priority`, `disabled`; and
      `[[upstreams]]` `auth = { mode = "xai_oauth", account | accounts }` — not both,
      `accounts = []` rejected, and a scope naming a missing file gives
      `xAI account scope references missing store account "<name>"` (502).
   5. *Validation*: the guard is unchanged (kind `responses`, https, x.ai/grok.com, no loopback
      exception) and covers every pooled account.
   6. *Selection*: the shared `AccountPool` — sticky per `x-claude-code-session-id`, round-robin
      otherwise, `priority`, `disabled`, admin pause, `ramp_initial_concurrency`. No quota windows,
      so no near-quota yield and `max_utilization*` stays inert. No re-probe, no WebSocket.
   7. *Failover*: the classification table from PLAN §1, with the 401 and 402 reasoning and the
      three reasons a 403 relays (tier gate surfaces at refresh; an inference 403 can be a
      content-policy refusal; nothing shared changes), plus the 403-body follow-up.
   8. *Refresh and isolation*: per-file store, per-file single-flight lock, the detached task, and no
      cross-write. Pooled accounts refresh through the redirect-hardened client, which refuses a
      redirect to non-loopback `http://`. The single-account file keeps the gateway's client.
   9. *Routed inbound Codex endpoint*: an `xai_oauth` route with a non-empty pool uses the first
      account in pool selection order (scope, `priority`, `disabled`, and pause honored), with one
      send, no rotation, and no cooldown. An empty pool uses the single-account file.
   10. *Response shaping*: Grok-CLI headers per selected account; `x-shunt-account` header and the
       `event: account` frame; use neutral account names on a shared gateway.
   11. *Admin and usage*: `GET /admin/api/pool`, `PATCH …/accounts/{ref}`, `GET /usage`; read-only,
       with no provisioning routes.
   12. *Not covered*: PLAN §4 items relevant to operators, and the deferred items from PLAN §5
       (Q1 follow-up, Q2, Q4 legacy hardening).
2. `docs/m6-xai-provider.md`:
   - §1 Scope: add the bullet
     "Multi-account pooling of `xai_oauth` (named accounts, rotation, failover) is specified in
     [`m18-xai-multi-account.md`](m18-xai-multi-account.md)."
   - End of §3: "Named pool accounts (`~/.shunt/accounts/xai/<name>.json`) use this same shape. This
     file is read only while that store is empty and no accounts are configured (M18)."
   - §4 step 4: "Persist to `~/.shunt/xai-auth.json` (§3), or with `--name <name>` to
     `~/.shunt/accounts/xai/<name>.json` (M18), and print …".
3. `docs/running.md`:
   - Line 223, `auth` row: replace "`xai_oauth` and `cursor_oauth` reuse their shunt-managed
     subscription logins." with "`xai_oauth` reuses `shunt login xai` and pools named accounts
     (`shunt login xai --name`; see [`m18-xai-multi-account.md`](m18-xai-multi-account.md));
     `cursor_oauth` reuses its shunt-managed subscription login."
   - Insert right before `### 5.4 Select a mapped model` (≈ line 779):

     ```markdown
     **Several SuperGrok accounts.** `shunt login xai --name <name>` stores a login at
     `~/.shunt/accounts/xai/<name>.json` (override the directory with `SHUNT_XAI_ACCOUNTS_DIR`). With no
     config change, an `xai_oauth` provider pools every account there and rotates on
     `401`/`402`/`429`/`5xx` (a `403` is relayed); scope it with `[[providers.grok.accounts]]` or
     `auth = { mode = "xai_oauth", accounts = [...] }`. `~/.shunt/xai-auth.json` is used only while that
     pool is empty. Spec: [`m18-xai-multi-account.md`](m18-xai-multi-account.md).
     ```

4. `docs/upstreams-failover.md`:
   - Row at line 74: the first cell becomes `` `claude_oauth` / `chatgpt_oauth` / `kimi_oauth` / `xai_oauth` ``.
     Replace its "No scope → … only source." sentences with: "No scope → whole store scan
     (`~/.shunt/accounts/kimi/` for `kimi_oauth`, `~/.shunt/accounts/xai/` for `xai_oauth`); for
     `chatgpt_oauth` an empty store additionally falls through to the single-account
     `~/.codex/auth.json` path, and for `xai_oauth` to `~/.shunt/xai-auth.json` (both preserve the
     earlier single-account behavior). `kimi_oauth` has no such single-account fallback — the
     shunt-managed store is its only source."
   - Row at line 75: remove `xai_oauth` from `` `xai_oauth` / `cursor_oauth` / `antigravity_oauth` ``.
   - Line 317: "A pooled (`chatgpt_oauth`) route's" → "A pooled (`chatgpt_oauth` or `xai_oauth`) route's".
5. `docs/m11-inbound-codex-endpoint.md`, the bullet "**One credential, no pool, no failover.**"
   (≈ line 176): append "For an `xai_oauth` route whose account pool is non-empty, that credential
   is the first account in pool selection order (the provider's `account`/`accounts` scope honored);
   an empty pool uses `~/.shunt/xai-auth.json` ([M18](m18-xai-multi-account.md))."

**Acceptance**

- `git status --short docs` shows changes only in these five files, plus the untracked
  `docs/plans/2026-10-08_xai-account-pool/`.
- `grep -n "SHUNT_XAI_ACCOUNTS_DIR" docs/m18-xai-multi-account.md docs/running.md` has hits in both.
- `grep -n "m18-xai-multi-account" docs/m6-xai-provider.md docs/running.md docs/m11-inbound-codex-endpoint.md`
  has hits in all three.
- Every relative link in `m18` resolves to an existing file (`ls` each target).

---

## T12 — Docs: site (English), README.md, grok blueprint

- **depends_on:** T11
- **files:** `site/src/content/docs/guides/xai.mdx` (primary), `site/src/content/docs/guides/providers.mdx`,
  `site/src/content/docs/guides/inbound-codex-endpoint.md`, `site/src/content/docs/reference/configuration.md`,
  `site/src/content/docs/reference/cli.mdx`, `site/src/content/docs/reference/endpoints.md`, `README.md`,
  `blueprints/upstream/grok.md`

**Instructions**

1. `guides/xai.mdx`:
   - "How it works" table, `Auth` row, `grok` cell: "Grok CLI OAuth from `~/.shunt/xai-auth.json`, or
     a pool of named logins under `~/.shunt/accounts/xai/`, auto-refreshed".
   - Aside "A different auth-file location": "for CI, a sandbox, or a second account" → "for CI or a
     sandbox. For a second account, use a named login — see *Several SuperGrok accounts* below."
   - Insert this section after Path A's `### 3. Route a model to grok` block, before `## Path B`:

     ~~~~mdx
     ## Several SuperGrok accounts

     shunt can pool several SuperGrok logins behind the `grok` provider and fail over between them, the
     way it pools Claude, Codex, and Kimi accounts. Log each one in under its own name:

     ```bash
     shunt login xai --name main
     shunt login xai --name backup
     ```

     Each login is saved to **`~/.shunt/accounts/xai/<name>.json`** (`0600`, in a `0700` directory;
     override the directory with `$SHUNT_XAI_ACCOUNTS_DIR`). With no further config, `grok` uses every
     account in that directory. To pick some, list them on the provider:

     ```toml
     [[providers.grok.accounts]]
     name = "main"

     [[providers.grok.accounts]]
     name = "backup"
     priority = 200          # optional: lower is preferred (default 100)
     ```

     In an `[[upstreams]]` config, name them in the auth map instead:
     `auth = { mode = "xai_oauth", accounts = ["main", "backup"] }` (or `account = "main"` for one).

     shunt keeps a session (`x-claude-code-session-id`) on one account and round-robins otherwise. When
     an account answers `401`, `402`, `429`, or `5xx`, or its token cannot be refreshed, shunt cools it
     down and retries the request on the next one. Any other `4xx`, including `403`, is returned as is:
     a `403` can be a content-policy refusal, and re-sending it to every account could get all of them
     flagged. The serving account is named in the `x-shunt-account` response header, or in an
     `account` event before a streamed reply.

     <Aside type="caution" title="The single-account file steps aside">

     `~/.shunt/xai-auth.json` (from `shunt login xai` without `--name`) is used only while the pool is
     empty. Once a named account exists, shunt stops reading it. To keep that login in the pool, stop
     shunt and move the file into the store; the format is the same:

     ```bash
     mv ~/.shunt/xai-auth.json ~/.shunt/accounts/xai/<name>.json
     ```

     Do not copy it. xAI rotates the refresh token on every refresh, so two files holding one login
     invalidate each other.

     </Aside>
     ~~~~

   - Troubleshooting:
     - Append to the `403 … not authorized for API access` row: "With several accounts, a tier gate
       that shows up when shunt refreshes an account's token cools that account down, and shunt moves
       to the next one. A `403` on a request is returned as is, because it can also be a
       content-policy refusal."
     - Add two rows:
       - `` `all upstreams failed (N attempted)` with named xAI accounts `` — "Every pooled account failed
         to load or refresh its credential. Run `shunt login xai --name <account>` again for each, or
         check the files under `~/.shunt/accounts/xai/`."
       - `` `xAI account scope references missing store account "<name>"` `` — "`account`/`accounts`
         names an account with no file in the store. Log it in with
         `shunt login xai --name <name>` or fix the name."
2. `guides/providers.mdx`:
   - Line 23, `grok` row: "reuses `~/.shunt/xai-auth.json`, or pools named logins under
     `~/.shunt/accounts/xai/` (`shunt login xai --name`)".
   - After the sentence at line 268, add: "To pool several SuperGrok accounts, log each in with
     `shunt login xai --name <name>`; see [xAI / Grok](/guides/xai/)."
3. `guides/inbound-codex-endpoint.md`, the bullet "**One credential, no failover.**" (≈ line 163):
   append "For an `xai_oauth` route with named accounts, that credential is the first account in
   pool selection order (the provider's `account`/`accounts` scope honored); with no named accounts
   it is `~/.shunt/xai-auth.json`."
4. `reference/configuration.md`:
   - Line 520: "`claude_oauth`, `chatgpt_oauth`, `kimi_oauth`, `xai_oauth`, and `antigravity_oauth` maps
     may select …". After the Antigravity fallback clause, add "; if the xAI store is empty and nothing
     is configured, `xai_oauth` falls back to `~/.shunt/xai-auth.json`". Change "`passthrough`,
     `xai_oauth`, and `cursor_oauth` maps take only `mode`" to "`passthrough` and `cursor_oauth` maps
     take only `mode`".
   - Line 556: replace "`xai_oauth` reuses `~/.shunt/xai-auth.json` from `shunt login xai` (only sent
     to x.ai/grok.com hosts over HTTPS)" with "`xai_oauth` pools named logins from
     `~/.shunt/accounts/xai/` (`shunt login xai --name`) and otherwise reuses `~/.shunt/xai-auth.json`
     from `shunt login xai` (only sent to x.ai/grok.com hosts over HTTPS)".
   - Line 559: add `"xai_oauth"` to the list of modes `accounts` is valid with. Leave the other
     entries as they are.
   - Line 576: "The `claude_oauth` / `chatgpt_oauth` / `kimi_oauth` / `xai_oauth` account pools …".
5. `reference/cli.mdx`:
   - Replace the `## shunt login xai` section body (lines 224-230) with:

     ~~~~mdx
     Run xAI's device-code OAuth flow and save its refreshable credential:

     ```bash
     shunt login xai
     shunt login xai --name <account-name>
     ```

     Without `--name` the credential lands in `~/.shunt/xai-auth.json` (override with
     `SHUNT_XAI_AUTH_FILE`). With `--name` it lands at `~/.shunt/accounts/xai/<account-name>.json` (0600,
     in a 0700 directory; `SHUNT_XAI_ACCOUNTS_DIR` overrides the store) instead, so named logins never
     overwrite the single-account file. Reusing a name replaces its file. `--mode` and `--long-lived`
     are rejected, since xAI OAuth tokens are always refreshable; `--manual` is claude-only.

     Reference named accounts from a provider with `[[providers.grok.accounts]]` entries or
     `auth = { mode = "xai_oauth", accounts = [...] }`, or configure none to scan the whole store. With
     no configured accounts and an empty store, the provider falls back to the single-account file;
     once any named account exists, that file is no longer read. Both files use the same format, so
     with shunt stopped, `mv ~/.shunt/xai-auth.json ~/.shunt/accounts/xai/<account-name>.json` moves an
     existing login into the pool.

     xAI rotates the refresh token on every refresh, so the one-owner-per-refreshable-login rule above
     applies: never copy one credential file to two places or share it between two running shunt
     processes.
     ~~~~

   - Environment table: after the `SHUNT_KIMI_ACCOUNTS_DIR` row, add
     `` | `SHUNT_XAI_AUTH_FILE` | Alternate credential file for `shunt login xai` without `--name` (default `~/.shunt/xai-auth.json`) | ``
     and `` | `SHUNT_XAI_ACCOUNTS_DIR` | Alternate shunt-managed xAI account-store directory | ``.
6. `reference/endpoints.md`: in the `/admin/api/pool` row (line 33),
   `claude_oauth`/`chatgpt_oauth`/`kimi_oauth`/`antigravity_oauth` →
   `claude_oauth`/`chatgpt_oauth`/`kimi_oauth`/`xai_oauth`/`antigravity_oauth`. Run
   `grep -n "kimi_oauth" site/src/content/docs/reference/endpoints.md` and add `xai_oauth` to any
   other enumeration of pooled modes for `PATCH /admin/api/pool/…` or `/usage`.
7. `README.md` line 132, `grok` row backend cell: "`cli-chat-proxy.grok.com/v1` — the Grok CLI proxy;
   reuses `~/.shunt/xai-auth.json` (`shunt login xai` with a SuperGrok / X Premium+ subscription), or
   named accounts under `~/.shunt/accounts/xai` (`shunt login xai --name`, pooled with failover)".
8. `blueprints/upstream/grok.md`, `## Credentials` section: append one paragraph modeled on
   `blueprints/upstream/kimi-code.md:42-60`. `shunt login xai --name <account-name>` stores at
   `~/.shunt/accounts/xai/<account-name>.json` (`SHUNT_XAI_ACCOUNTS_DIR`); with no further config the
   upstream uses every stored account, or scope it with
   `auth = { mode = "xai_oauth", accounts = ["<a>", "<b>"] }` (or `account = "<a>"`), not both. While
   any named account exists, the single-account file is not read. Never share a refreshable credential
   file between concurrently running shunt processes.

**Acceptance**

- `git status --short site README.md blueprints` shows changes only in these eight files.
- `grep -n "SHUNT_XAI_ACCOUNTS_DIR" site/src/content/docs/reference/cli.mdx site/src/content/docs/guides/xai.mdx blueprints/upstream/grok.md`
  has hits in all three.
- `grep -n "xai_oauth" site/src/content/docs/reference/endpoints.md site/src/content/docs/guides/inbound-codex-endpoint.md`
  has hits in both (the endpoints hit in the `/admin/api/pool` row).
- `cargo test --all-features --test add_cli`
  (the blueprint is `include_str!`'d).
- `cd site && npm ci && npm run build` succeeds.
  If `npm ci` cannot reach the network, say so in the report instead of skipping silently.

---

## T13 / T14 / T15 — Docs: translations (one ticket per locale)

- **T13 = `ko`, T14 = `ja`, T15 = `zh-cn`.** Each depends on T12; they run in parallel.
- **files** (replace `<l>` with the locale, and `<L>` with `ko`, `ja`, or `zh-CN`):
  `site/src/content/docs/<l>/guides/xai.mdx` (primary), `site/src/content/docs/<l>/guides/providers.mdx`,
  `site/src/content/docs/<l>/guides/inbound-codex-endpoint.md`,
  `site/src/content/docs/<l>/reference/configuration.md`, `site/src/content/docs/<l>/reference/cli.mdx`,
  `site/src/content/docs/<l>/reference/endpoints.md`, `README.<L>.md`

**Instructions**

1. Nothing is committed yet, so the English changes to translate come from `origin/main`
   (review decision D7):

   ```bash
   git diff origin/main -- \
     site/src/content/docs/guides/xai.mdx site/src/content/docs/guides/providers.mdx \
     site/src/content/docs/guides/inbound-codex-endpoint.md \
     site/src/content/docs/reference/configuration.md site/src/content/docs/reference/cli.mdx \
     site/src/content/docs/reference/endpoints.md README.md
   ```

   Translate **only** those changes into the matching place in each locale file. The locale pages
   are not line-for-line copies: find the matching paragraph, table row, or section by content, not
   by line number.
2. Keep code blocks, TOML, commands, paths, env var names, status codes, and quoted error strings
   (`xAI account scope references missing store account`, `all upstreams failed (N attempted)`)
   verbatim in English.
3. Anchors do not survive translation (AGENTS.md "Documentation"). Do not add a `#fragment` link to
   another page. Link the page root (e.g. `/<l>/guides/xai/`, matching how the file already links
   pages), or take the id from `site/dist/<l>/…/index.html` after `npm run build`. The in-page
   reference "see *Several SuperGrok accounts* below" stays plain text, not a link.
4. Match each file's existing register and terminology (read the surrounding section first).
5. **ko only:** check every particle that follows a code span or English word (e.g. `--name`을/를,
   `xai_oauth`은/는). Choose it by how the preceding token is read aloud (the last syllable's final
   consonant). List each such sentence in the report so a reviewer reads it.

**Acceptance**

- `git diff --stat origin/main -- site/src/content/docs/<l> README.<L>.md` lists only the seven
  files above for that locale.
- `grep -c "SHUNT_XAI_ACCOUNTS_DIR" site/src/content/docs/<l>/reference/cli.mdx` ≥ 2 (both the
  section and the env table).
- `grep -n "xai_oauth" site/src/content/docs/<l>/reference/endpoints.md site/src/content/docs/<l>/guides/inbound-codex-endpoint.md`
  has hits in both.
- `grep -n "accounts/xai" README.<L>.md` has a hit in the `grok` row.
- `git diff -U0 origin/main -- site/src/content/docs/<l> | grep -n "](/[^)]*#"` prints nothing (no
  new cross-page fragment links).
- `cd site && npm run build` succeeds (or report
  why it could not run).
