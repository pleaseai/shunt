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

Create a shunt-managed Anthropic pool account:

```bash
# Import the current refreshable Claude Code login.
shunt login claude --name primary

# Run Claude's one-year setup-token flow and store the result.
shunt login claude --name ci --long-lived
```

The default form copies `~/.claude/.credentials.json` (or `CLAUDE_CREDENTIALS`) into `~/.shunt/accounts/claude/<name>.json`. It preserves refresh tokens, and shunt refreshes that private copy rather than changing Claude Code's source file.

`--long-lived` requires the `claude` executable in `PATH`. shunt runs the official interactive `claude setup-token` browser flow on your terminal, then asks you to paste its token into a hidden prompt. shunt never prints the token. The file is written atomically at `0600` inside a `0700` directory on Unix. `SHUNT_CLAUDE_ACCOUNTS_DIR` overrides the store directory; reusing a name replaces its file.

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

For an Anthropic provider with `auth = "claude_oauth"`, an account can use a name-only store entry, `credentials = "~/.claude/.credentials.json"`, or `token_env = "YOUR_ENV_NAME"`. See [Anthropic Multi-Account](/guides/anthropic-multi-account/) for complete configuration and failover rules.

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
