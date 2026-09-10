# Offline OpenCodex credential import

`shunt import opencodex --dry-run` previews compatible active credentials without
displaying their values or writing files. Repeat `--provider NAME` to select
providers. Interactive export asks for confirmation; automation must pass
`--yes`. Use `--from PATH` for a securely transferred copy on another machine.
Without it the source is `OPENCODEX_HOME`, then `~/.opencodex`.

Each export creates `~/.shunt/imports/opencodex-UUID/credentials.env` (or beneath
`--output-dir`). Directories are 0700 and files 0600 on Unix. Existing snapshots,
Shunt credential files, shell profiles and configuration remain untouched; there
is no replacement mode. This preserves previous state instead of overwriting it
and attempting recovery from a backup. Keep this directory out of Git and use
encrypted transfer when moving credentials between machines.

Source the exact generated file path before starting Shunt, for example:

```bash
source /path/printed/by/import/credentials.env
shunt run --config ./shunt.toml
```

No provider or model routing is imported. API keys must be mapped to the emitted
environment variable using the existing `api_key_env` setting. OpenAI, xAI and
Command Code API keys map to `OPENAI_API_KEY`, `XAI_API_KEY` and
`SHUNT_COMMANDCODE_API_KEY`; other provider identifiers become uppercase
`SHUNT_IMPORTED_<PROVIDER>_API_KEY` with hyphens replaced by underscores.
Collisions are refused, not silently overwritten. Sourcing a snapshot explicitly
replaces matching variables in that shell; it does not change credential files.

## Supported source contracts

Source schema inspected at OpenCodex revision
`055c3ecf0de6c35f59195fc434d6b08525182b7f` on 2026-09-09:

- `src/types/provider.ts`: `config.json.providers.NAME.apiKey` is the active
  key. Only enabled `anthropic`, `openai-chat` and `openai-responses` adapters
  are accepted. Key pools are not guessed and top-level gateway `apiKeys` are
  never treated as upstream credentials.
- `src/oauth/types.ts`, `src/oauth/store.ts`: `auth.json` contains legacy
  credentials or `{activeAccountId, accounts:[{id, credential, needsReauth}]}`.
  The selected active account must be unique and not need reauthentication.
  Only `cursor` and `command-code` are exported as
  `SHUNT_CURSOR_AUTH_TOKEN` and `SHUNT_COMMAND_CODE_TOKEN` respectively.
  `credential.access` must be header-safe; `expires` is epoch milliseconds
  and must be at least one minute in the future. No refresh tokens are copied.

These are access-token snapshots, not refreshable migrations. Re-import after
expiry or log in separately. Claude, Codex account pools, Antigravity, Kimi, Kiro,
cookies and unknown OAuth formats are not imported by this version. Importing
an API key does not change provider admission: OpenCode Go remains unadmitted.
No network validation or claim of live provider availability is made.

## Safety and verification

The importer reads raw bounded JSON (4 MiB per file), never executes the OpenCodex
loader, and does not repair, migrate, refresh, chmod or write the source. JSON
parse errors do not echo source values. Unix reads reject symlinks and special
files; output inside the source tree is refused. Shell values are quoted, never
executed during import. Export requires Unix owner-only permissions; preview is
available on other platforms. Test only with synthetic `--from` and
`--output-dir` paths under the committed isolated runner, never the live home.

`tests/import_opencodex.rs` covers the CLI, explicit confirmation, redaction,
active-account selection, access-only export, expiry, malformed input, source
preservation, permissions, shell metacharacters, collisions and output separation.
README and CLI reference changes are maintained in English, Korean, Japanese and
Simplified Chinese. Generated wiki files are not edited.

### Verified implementation (2026-09-09)

- Format check and warnings-denied all-target/all-feature Clippy passed.
- Full all-feature workspace suite passed, including all seven importer CLI
  tests; the two pre-existing ignored tests are unchanged. No assertions were
  weakened and no retries were needed.
- All five mock gateway smoke checks passed on isolated ports 31711/31712.
- Site build passed: 173 pages, four locales. The importer section was verified
  in each rendered CLI reference. Existing Vite deprecation and Pagefind language
  stemming warnings remain nonfatal.
- All stateful checks inherited fresh isolated homes. Production OpenCodex
  config mtime/SHA-256 and invalid/backup inventory remained unchanged. Only
  synthetic credentials were used; no real import or live provider call occurred.

The existing intermittent Antigravity timeout is not claimed fixed by this work.
Neither runtime refresh behavior nor provider admission was changed.
