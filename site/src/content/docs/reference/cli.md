---
title: CLI
description: The shunt command line — run, check, token, and provider login.
---

## `shunt run`

Start the gateway. `run` is the default subcommand, so a bare `shunt` also works.

```bash
shunt run
shunt run --config /path/to/shunt.toml
```

On start it logs `shunt listening` with the bound address (default `127.0.0.1:3001`). Set log verbosity with `RUST_LOG`, e.g. `RUST_LOG=shunt=debug shunt run`.

Config files may be TOML or YAML, chosen by extension (`.toml`, or `.yaml`/`.yml`). Without `--config`, shunt probes each directory for `shunt.toml` → `shunt.yaml` → `shunt.yml` across `./` → `~/.config/shunt/` → `$HOMEBREW_PREFIX/etc/`; with `--config`, a missing file is an error. See [Configuration](/guides/configuration/).

## `shunt check`

Validate the resolved configuration and exit (`shunt --check` also works):

```bash
shunt check
# -> config ok
```

Reports specific errors: a bad bind address, an unknown provider in a route, a missing `api_key_env`, a bad `base_url`, a wrong adapter/auth combination.

## `shunt token`

Print a Claude subscription OAuth token to **stdout** (logs go to stderr), designed to be wired into Claude Code's `apiKeyHelper`. Two modes:

- **Static** — if `SHUNT_GATEWAY_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN` is set, echoes that value unchanged. Point it at a `claude setup-token` value and nothing is ever refreshed.
- **Auto-refresh** — otherwise reads `~/.claude/.credentials.json` (override the path with `CLAUDE_CREDENTIALS`), returns the `claudeAiOauth` access token, and when it is within 5 minutes of `expiresAt` refreshes it against `platform.claude.com/v1/oauth/token` (the same grant Claude Code uses), then writes the new token back atomically at `0600`, preserving every other field. Refresh happens only on actual expiry, to respect the endpoint's rate limit.

```json
// ~/.claude/settings.json
{
  "apiKeyHelper": "/path/to/shunt token"
}
```

See [Connect Claude Code](/guides/connect-claude-code/#2-choose-the-anthropic-credential) for when you need this.

## `shunt login claude`

Create a shunt-managed Anthropic pool account with one of three modes:

```bash
# Full OAuth: shunt obtains and stores a new refreshable login (recommended).
shunt login claude --name primary --mode oauth

# Import the current refreshable Claude Code login.
shunt login claude --name imported --mode import

# Run Claude's one-year, inference-only setup-token flow.
shunt login claude --name ci --mode setup-token
```

When `--mode` is omitted on a TTY, shunt prompts for `oauth`, `import`, or `setup-token` and recommends OAuth by default. In non-interactive input it keeps the historical `import` default. `--long-lived` remains a deprecated alias for `--mode setup-token`.

`--mode oauth` runs shunt's full-scope PKCE authorization flow and stores both access and refresh tokens. By default shunt binds an ephemeral listener to `127.0.0.1`, opens the authorization URL, and finishes when the browser returns to `http://127.0.0.1:<port>/callback`. If the browser cannot open, the listener cannot start, or no callback arrives within 5 minutes, it falls back to the hidden manual-paste flow. Pass `--manual` to use that flow immediately, which is useful over SSH or in a headless environment:

```bash
shunt login claude --name remote --mode oauth --manual
```

`--mode import` copies `~/.claude/.credentials.json` (or `CLAUDE_CREDENTIALS`) into `~/.shunt/accounts/claude/<name>.json`. It preserves refresh tokens, associates the copy with the current account UUID from Claude Code's global configuration, and shunt refreshes that private copy rather than changing Claude Code's source file.

`--mode setup-token` runs the same one-year, inference-only PKCE flow as `claude setup-token`. After browser approval, paste the displayed authorization code into shunt's hidden prompt; shunt exchanges it directly and stores both the opaque token and the issuing account UUID, never printing the token.

The file is written atomically at `0600` on Unix; a store directory that shunt creates is made with `0700`, but a pre-existing `SHUNT_CLAUDE_ACCOUNTS_DIR` override directory keeps its own permissions. `SHUNT_CLAUDE_ACCOUNTS_DIR` overrides the store directory; reusing a name replaces its file. External setup tokens supplied via `token_env` may add an optional `uuid`; it is only needed to rewrite the request's embedded account UUID, which cannot be recovered after issuance.

:::caution[One owner per refreshable login]
OAuth providers may rotate the refresh token whenever shunt refreshes an access token. Do not run the same refreshable credential file in multiple shunt processes, and do not copy a live refreshable store file into another independently running host. The first refresh can invalidate the other copy. Provision each process separately, or use a non-refreshable setup token where shared static credentials are appropriate.
:::

Reference the result with a name-only pool entry, or leave the provider's account list empty to scan every store file:

```toml
[[providers.anthropic.accounts]]
name = "primary"
```

## `shunt login xai`

Run xAI's device-code OAuth flow and save its refreshable credential:

```bash
shunt login xai
```

## Anthropic account-pool authentication

For an Anthropic provider with `auth = "claude_oauth"`, an account can use a name-only store entry, `credentials = "~/.claude/.credentials.json"`, or `token_env = "YOUR_ENV_NAME"`. The store entry can come from full OAuth, an imported Claude Code login, or the setup-token flow described above. See [Anthropic Multi-Account](/guides/anthropic-multi-account/) for complete configuration and failover rules.

## Environment variables

| Variable | Effect |
| :-- | :-- |
| `SHUNT_*` (e.g. `SHUNT_SERVER__BIND`) | Override any config key; `__` separates nested keys |
| `RUST_LOG` | Log filter, e.g. `shunt=debug` |
| `SHUNT_CLIENT_TOKENS` | Client tokens for [`[server.auth]`](/guides/shared-gateway/) (name configurable via `tokens_env`) |
| `SHUNT_GATEWAY_TOKEN` / `CLAUDE_CODE_OAUTH_TOKEN` | Static token for `shunt token` |
| `CLAUDE_CREDENTIALS` | Alternate credentials file path for `shunt token` and refreshable `shunt login claude` import |
| `SHUNT_CLAUDE_ACCOUNTS_DIR` | Alternate shunt-managed Claude account-store directory |
| Account-specific variable named by `token_env` | Setup token for an Anthropic `claude_oauth` pool entry; used verbatim |
| `OPENAI_API_KEY` | Default key env for the `openai` provider (per-provider via `api_key_env`) |
