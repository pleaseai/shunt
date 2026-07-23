# Desktop app — Tauri UI for upstream/account management

A native desktop application that lets a single operator run shunt locally and
manage its **upstreams**, **accounts**, and observe **pool health** from a GUI —
without hand-editing `shunt.toml` or running CLI login flows in a terminal.

This document is the design record. It fixes the framework decision (Tauri), the
architecture (a thin shell over a shunt sidecar, growing native config-editing
commands), and the two hard problems the desktop surface must solve that the
existing `[server.admin]` web surface ([`m9-admin-surface.md`](m9-admin-surface.md))
does not: **writing** config and **injecting** env-held secrets.

## Motivation

shunt today has two provisioning surfaces:

- **CLI** — `shunt login`, `shunt init`, `shunt add`, `shunt check`, `shunt run`.
- **Admin web** (`[server.admin]`, M9) — a browser surface for adding Claude/Codex
  accounts and a read-only pool dashboard, designed for a *remote* deployed
  gateway reached over HTTPS/a tunnel.

Neither covers the **local single-user** case well:

- The admin web offers **full CRUD for Claude/Codex account-store files** (add,
  list, remove, replace) but **never writes the config file**, and there is no
  upstream CRUD anywhere. Upstreams (`[[upstreams]]`) can only be added, removed,
  or reordered by editing `shunt.toml` by hand.
- The admin web **cannot set secrets**: API keys and tokens live in the process
  environment (`api_key_env`, `tokens_env`), which a running remote server cannot
  rewrite for itself.
- Running shunt as a personal local gateway (the common Claude Code / Codex CLI
  use case) still means a terminal, a hand-written config, and manual restarts.

A desktop app owns the whole local lifecycle: it starts/stops the shunt process,
owns the config file, owns the secret store, and injects env at spawn time. That
makes upstream CRUD and secret entry — impossible for the remote admin web —
straightforward.

## Framework decision: Tauri

Evaluated Tauri vs Deno Desktop (the two candidates the operator asked to
compare), against Electron/Electrobun as references.

| | Tauri | Deno Desktop | Electron |
| :-- | :-- | :-- | :-- |
| Backend language | **Rust** | JS/TS (Deno) | JS/TS |
| Bundle size | ~2–10 MB min / ~57 MB real | ~40 MB (WebView) / ~150 MB (CEF) | 100 MB+ |
| Idle memory | ~109 MB | ~98 MB | ~128 MB |
| Render engine | OS WebView | OS WebView / CEF | bundled Chromium |
| Installers | built-in (MSI/DEB/RPM/DMG) | partial | mature |
| Secure storage | plugin (keychain/stronghold) | **none yet** | third-party |
| Maturity | stable | **early (Deno 2.9+), "not production-ready"** | most mature |

Sources: BetterStack macOS benchmark and Deno's own desktop comparison doc.

For shunt specifically, three project facts make Tauri the clear choice — beyond
the generic size/memory numbers:

1. **Language match.** shunt is Rust and already exposes a library target
   (`[lib] name = "shunt"` in `Cargo.toml`). Tauri commands can call
   `config.rs`, the account stores, and validation **directly**. Deno would force
   reimplementing that logic in TS or talking to shunt only over HTTP.
2. **Secret storage.** shunt's core job is holding OAuth tokens and API keys.
   Tauri has a keychain/stronghold plugin; **Deno Desktop has no secure-storage
   API yet** (per its own docs). This is disqualifying for shunt.
3. **Minimal binary + built-in cross-platform installers**, suited to a
   tray-resident helper. Deno's own doc recommends Tauri when "minimal binary
   size" matters and notes its installer support is only partial and Windows
   auto-update is missing.

Deno's advantages (staying in JS/TS, npm/native-Node modules, consistent CEF
rendering) do not apply here. **Decision: Tauri.**

## Scope

In scope:

- **Process lifecycle.** Start/stop/restart the shunt gateway; show run state and
  the effective config path; tray icon + menu; launch-at-login (optional).
- **Upstream CRUD.** Add, edit, remove, and reorder `[[upstreams]]` entries via a
  form, written back to the config file (the new capability).
- **Account management.** Add/remove Claude and Codex/ChatGPT accounts, reusing
  the existing OAuth/setup-token flows and stores. API-key upstreams accept a key
  entered in the UI, stored in the OS keychain.
- **Pool dashboard.** The read-only per-account quota/cooldown view
  (`AccountPool::snapshot`), same data as `GET /admin/pool`.
- **Config safety.** Every write is validated (the `shunt check` logic) before it
  lands; a rejected edit never reaches the file.

Out of scope (initially):

- Multi-user / RBAC — the desktop app is single-operator, local-machine.
- Remote gateway management — the app manages a *local* shunt process. (Pointing
  it at a remote shunt over the admin API is a possible later mode, not v1.)
- Editing every config key. v1 targets upstreams, accounts, and a few server
  fields (bind, auth); the rest stay file-edited with hot-reload.
- Mobile targets, auto-update (deferred; Tauri supports both later).

## Architecture

A **hybrid, incremental** shape: ship a working shell fast, then grow native
config editing. The two are not exclusive — the shell reuses the existing admin
web while native commands replace screens one at a time.

```
┌─────────────────────────────────────────────┐
│ Tauri app  (desktop/ — separate crate)       │
│                                              │
│  Tray + window (webview frontend)            │
│   ├─ Phase A: embeds the shunt admin web     │
│   └─ Phase B: native screens (Vue/Svelte…)   │
│                                              │
│  Rust side (#[tauri::command]):              │
│   ├─ lifecycle: spawn/stop shunt sidecar     │
│   ├─ config: read + write shunt.toml         │
│   │    (toml_edit, format-preserving)        │
│   ├─ secrets: OS keychain ↔ env injection    │
│   └─ validate: shunt's Config::load path     │
│                                              │
│         │ spawns, injects env                │
│         ▼                                    │
│  ┌────────────────────────────────────────┐ │
│  │ shunt gateway (sidecar process)         │ │
│  │  - loads shunt.toml (figment)           │ │
│  │  - file-watch hot-reload on our writes  │ │
│  │  - loopback bind (127.0.0.1)            │ │
│  └────────────────────────────────────────┘ │
└─────────────────────────────────────────────┘
```

### Embedding shunt: sidecar vs library link

Two ways to run the gateway from the app:

- **Sidecar (recommended for v1).** Bundle the compiled `shunt` binary as a Tauri
  resource and spawn it (`tauri-plugin-shell` / `Command`). shunt runs exactly as
  it does standalone: its own bootstrap (Sentry, telemetry, SIGHUP + file-watch
  reload). The app injects env and points `--config` at the managed file. Simple,
  faithful, and shunt stays a single source of truth. Downside: process boundary,
  version-pinning the bundled binary.
- **Library link (possible later).** Depend on the `shunt` lib crate and call
  `server::build_router` on a tokio runtime inside the app. Tighter integration,
  no second process — but the app must reproduce main.rs's bootstrap and own the
  reload loop. Defer; revisit if the sidecar boundary becomes limiting.

v1 uses the **sidecar**. Only **config-file editing and validation** run
in-process (the file is on disk, shared with the sidecar); everything that touches
the sidecar's **live runtime state** — account provisioning and the pool snapshot —
goes through the sidecar's admin HTTP surface, because that state lives in the
sidecar process, not the desktop app. So: "sidecar for serving and for runtime
state; in-process only for editing the shared config file."

### Phase A — shell + lifecycle (fastest working app)

- Tray app that spawns/stops the shunt sidecar and shows run state.
- The window embeds `http://127.0.0.1:<port>/admin` (existing M9 HTML). Reuses
  account add/remove and the pool dashboard immediately.
- App owns the config path (default `$XDG_CONFIG_HOME/shunt/shunt.toml`, the same
  location `Config::find_config_file` searches) and generates the
  `SHUNT_ADMIN_TOKENS` value it injects into the sidecar. Injecting that env only
  lets the sidecar *validate* the token — the embedded webview must still
  authenticate, or `/admin` redirects to `/admin/login`. So the app logs in on the
  operator's behalf: it POSTs the token to `/admin/login` to obtain the session
  cookie for the webview (or attaches the admin header to webview requests). The
  operator does no manual login either way.

### Phase B — native upstream/account CRUD

- Native frontend screens for upstreams, accounts, and pool. `#[tauri::command]`s
  write the config **file** in-process (upstream CRUD), while account provisioning
  and the pool snapshot call the sidecar's admin HTTP (their state is in the
  sidecar, and the handlers/helpers are not exported — see below).
- Replaces the embedded admin web screen-by-screen; the admin web remains for
  remote deployments and as a fallback.

## Hard problem 1 — writing config

shunt **reads** config through figment (`Config::load`): it deep-merges built-in
defaults + the file (`shunt.toml`/`.yaml`/`.yml`) + `SHUNT_`-prefixed env into a
`Config`. There is **no serialization/round-trip path** — the runtime never
writes the file back. The desktop app must own writing.

Constraints:

- **Preserve the user's file.** Operators also hand-edit `shunt.toml` (comments,
  key order, formatting). A naive `toml::to_string(&config)` round-trip through
  serde would flatten comments and reorder keys, and would bake env-overridden
  values into the file. Use **`toml_edit`** (format-preserving TOML) to mutate
  only the `[[upstreams]]` array-of-tables the UI touches, leaving the rest byte-
  identical. (`UpstreamConfig` already derives `Serialize` with
  `skip_serializing_if` on optionals, so rendering a *new* entry is clean; editing
  an existing one is a keyed `toml_edit` mutation.)
- **Validate before write.** Run the candidate *file text* through the full
  `Config::load` path — the figment parse (which catches TOML syntax errors,
  `deny_unknown_fields`, wrong declaration form, and env-merge errors) **plus**
  `Config::validate` — before touching the file. That is exactly what `shunt
  check` runs. `Config::validate` alone operates on an already-parsed `Config`, so
  on its own it would miss the malformed-file cases the UI most needs to reject.
  Even so, the reload path is fail-safe: an invalid file that somehow lands leaves
  the running config unchanged.
- **Atomic write + hot-reload.** Write to a temp file and rename over the target
  (`atomic_file.rs` already exists in the tree). shunt watches the parent
  directory with a 400 ms debounce and detects atomic renames, so the write
  hot-applies with no restart for reload-safe fields.
- **YAML.** The same file may be `shunt.yaml`. v1 can standardize the managed file
  as TOML and note YAML as file-edit-only, or add a YAML-preserving writer later.
  (Decision below in Open questions.)

### Reload vs restart matrix

The app must tell the user when an edit is live immediately vs needs a restart,
matching shunt's actual reload semantics ([`config-reload.md`](config-reload.md)):

| Change | Applies on | Notes |
| :-- | :-- | :-- |
| Add/edit/remove/reorder `[[upstreams]]` | **reload** (file write) | hot, no restart |
| Add/remove account (OAuth store file) | **next request** | the store dir's mtime changes and the account is rescanned on the next request; no config write, so the reload path is not triggered at all. Explicitly-scoped `accounts` entries still need a config edit + reload |
| `[server.auth]` tokens, routes, models | **reload** | token/header edits within an already-enabled surface hot-apply |
| `server.bind` (port/host) | **restart** | listener already bound |
| Toggle a boot-registered surface on/off — `[server.admin]`, `[server.gateway]`, `[server.codex_endpoint]`, `[server.usage]`, `[server.oauth_usage]` | **restart** | routes are registered once at boot; edits *within* an already-enabled surface still hot-apply |
| Any **env-held secret** (API key, admin token) | **restart** | see problem 2 |
| `[sentry]`, `[otel]` | **restart** | initialized once at startup |

The app drives restart itself (it owns the sidecar), so "requires restart" is a
one-click action, not a manual step.

## Hard problem 2 — secrets and env injection

shunt keeps secrets **out of the config file**: API keys and tokens are named env
vars, read from the process environment — but at **different times**. Config-backed
auth/OIDC secrets (`tokens_env = "SHUNT_ADMIN_TOKENS"`, `client_secret_env`, …) are
resolved on config **load and reload** (src/config.rs). API keys (an API-key
upstream's `auth.env`) and inline account `token_env` values are looked up **per
request** during credential resolution (src/auth/mod.rs). Either way the value
comes from the process's environment, which is **fixed at spawn** — a running shunt
cannot rewrite its own environment, so a rotated secret is not picked up until the
process is restarted with the new environment. OAuth account credentials are
different: they live in `0600` store files the stores already own.

The desktop model:

- **API keys / tokens → OS keychain.** When the operator enters an API key for an
  API-key upstream, the app writes the config with
  `auth = { mode = "api_key", env = "SHUNT_UPSTREAM_<name>_KEY" }` — `[[upstreams]]`
  carries the env name inside the `auth` map's `env` field, not the legacy
  top-level `api_key_env` of the `[providers.<name>]` form — and stores the
  **value** in the OS keychain via the Tauri secure-storage plugin. On spawn, the
  app reads the keychain and passes the value as that env var to the sidecar. The
  secret never touches the config file or logs.
- **OAuth accounts → existing stores.** Claude/Codex account provisioning reuses
  the M9 flows and their `0600` store writes unchanged. No new secret path.
- **Restart on secret change.** Because the sidecar's environment is fixed at
  spawn, changing a keychain-backed secret requires respawning the sidecar with the
  new environment. The app does this automatically (one click / automatic), so the
  "env can't hot-reload" limitation is invisible to the user.

This is precisely the capability the remote admin web lacks: a co-located process
owner that can rewrite the child's environment and restart it.

## UI surface (screens)

1. **Status / control** — run state, config path, port; Start/Stop/Restart;
   "restart required" banner when a pending change needs it.
2. **Upstreams** — ordered list (drag to reorder), add/edit/remove. Form fields
   map to `UpstreamConfig`: `name`, `provider`/`kind`, `base_url`, `auth`
   (passthrough / api_key / claude_oauth / chatgpt_oauth / xai_oauth /
   cursor_oauth), `effort`, `websocket`, `tool_search`, `retry`. Presets
   (`presets.rs`) prefill provider defaults.
3. **Accounts** — Claude and Codex accounts: add (OAuth/setup-token flow),
   remove; per-account metadata (name, kind, expiry) — never the token.
4. **Pool** — read-only quota/cooldown dashboard via `GET /admin/pool` on the
   sidecar (the endpoint returns `AccountPool::snapshot` data; the desktop process
   never calls the type directly, since the live pool is sidecar-local).
5. **Settings** — bind address, `[server.auth]` client tokens, admin token
   (auto-managed for the embedded web), log level.

## Tauri command surface (Phase B)

Native commands, mapping to existing shunt logic rather than the HTTP admin API:

| Command | Backing logic |
| :-- | :-- |
| `config_read` | `Config::load` + read raw file for `toml_edit` |
| `config_validate` | full `Config::load` (figment parse + `Config::validate`) |
| `upstream_upsert` / `upstream_remove` / `upstream_reorder` | `toml_edit` mutation + full `Config::load` validation + atomic write (in-process — the config file is shared on disk) |
| `account_add_claude` / `account_add_codex` / `account_remove` | **call the sidecar admin HTTP** (`POST`/`DELETE /admin/accounts/*`, carrying the configured admin header — see note below). The Claude OAuth helpers (`auth/claude/login.rs`) are crate-private and the Codex handlers (`admin/codex.rs`) are admin-module-private and need server state, so direct reuse from a separate `desktop/` crate would not compile unless shunt exports them (see Open questions) |
| `secret_set` / `secret_delete` | keychain plugin + env-name assignment |
| `pool_snapshot` | **`GET /admin/pool` on the sidecar** (with the configured admin header). `AccountPool` is the *sidecar* process's live runtime state (quota/cooldown accrued while serving), so calling `AccountPool::snapshot` from the desktop process would read an empty local pool, not the sidecar's |
| `gateway_start` / `gateway_stop` / `gateway_restart` | sidecar process control |

Every admin-HTTP command (`account_*`, `pool_snapshot`) sends the app's admin token
in the **configured admin header** — `x-shunt-admin-token` by default, or whatever
name the app wrote to `[server.admin].header` (the app owns the config, so it uses
the header name it set, not a hardcoded one). It is the same credential the app
generates, injects into the sidecar (`SHUNT_ADMIN_TOKENS`), and uses for the
embedded webview login. Header-token callers are CSRF-exempt (M9), so no session
cookie is needed; without the correct header the sidecar rejects the request with
HTTP 401.

## Security

Local single-user changes the model from the remote admin web:

- **Loopback only.** The sidecar binds `127.0.0.1`; no external surface by
  default. The heavy remote-admin hardening (CSRF, session cookies, OIDC,
  rate-limits) is unnecessary for a local IPC app and is not reimplemented for the
  native path. If Phase A embeds the admin web, it still uses the M9 auth, with an
  app-generated admin token injected via env.
- **Secrets in the OS keychain**, never in the config file or logs. Config writes
  only ever contain env *names*.
- **Process isolation** via the sidecar boundary; the app validates every write
  before it reaches the file.
- **Store file ownership.** Refresh-token rotation still requires a single active
  process owner of each `0600` store file (the M8/M9 hazard). The desktop app is
  that owner on the local machine; it must warn if a store is also driven by
  another running shunt.

## Build sequence

1. **Phase A — shell.** `desktop/` Tauri crate; tray + window; spawn/stop shunt
   sidecar; embed `/admin`; own the managed config path and injected admin token.
   Deliverable: a launchable app that runs shunt and shows the existing admin UI.
2. **Config writer.** `toml_edit`-based upstream read/write + full `Config::load`
   validation gate + atomic write; verify hot-reload picks it up. (This is the core
   new capability; both phases need it.)
3. **Phase B — native screens.** Upstreams CRUD, then accounts, then pool, each
   replacing the embedded page. Keychain-backed secret entry for API-key
   upstreams with automatic restart.
4. **Packaging.** Per-OS installers, code signing/notarization, optional
   launch-at-login and auto-update (deferred).

## Open questions

- **Managed file format.** Standardize the app-managed file as `shunt.toml` and
  treat YAML as file-edit-only, or invest in a YAML-preserving writer? (Lean: TOML
  only for v1.)
- **Sidecar vs lib link.** v1 sidecar; is there a concrete reason to link the lib
  earlier (e.g. sub-100 ms control latency, no bundled binary)?
- **Config-writer home.** Does the `toml_edit` writer + env-name assignment belong
  in the `shunt` crate (so the CLI can share it, e.g. a future `shunt upstream
  add`) or only in the desktop crate? Sharing it in the lib is likely worth it.
- **Exporting provisioning helpers.** v1 routes account provisioning and pool
  reads through the sidecar's admin HTTP, because the Claude/Codex provisioning
  helpers are crate/module-private and the pool is sidecar-local runtime state.
  Should shunt instead export those helpers (and an out-of-process way to read pool
  state) so Phase B can call them in-process, or is admin HTTP the intended
  long-term integration boundary?
- **Secret env-name scheme.** Confirm a collision-free, stable naming convention
  for the generated API-key env-var names (the `auth.env` value on an API-key
  upstream) and document it.

## Testing

- **Config writer (unit).** `toml_edit` upsert/remove/reorder preserves unrelated
  content and comments; round-trips through `Config::load`; rejects an edit that
  fails the full `Config::load` validation without writing; atomic write leaves no partial file.
- **Secret flow (unit).** env-name generation is stable and collision-free;
  keychain value never appears in the rendered config or logs.
- **Reload integration.** A written config is picked up by the file watcher
  (debounced), and a `server.bind`/secret change is correctly flagged
  restart-required.
- **Lifecycle (integration).** spawn → ready → stop; injected env reaches the
  sidecar; restart applies a rotated secret.
- **Frontend (e2e).** upstream add/edit/remove/reorder round-trips to the file and
  reflects in the pool/status views.

## Documentation impact

Per AGENTS.md, ship docs with the code:

- `README.md` — add the desktop app to the surfaces list (CLI / admin web /
  desktop) and a quickstart once Phase A lands.
- `docs/` — this file is the spec; update it as phases land and behavior settles.
- `site/` — a getting-started/guide page for installing and using the desktop app
  when it ships.
- If the `toml_edit` config writer moves into the `shunt` crate and gains a CLI
  entry point (`shunt upstream …`), document it under `docs/` + `site/reference`.
