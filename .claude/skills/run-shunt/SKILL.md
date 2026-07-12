---
name: run-shunt
description: Build, launch, and drive shunt — the Claude Code LLM gateway (a Rust/axum Anthropic-Messages proxy). Use to run, start, smoke-test, or curl-drive the gateway, exercise /v1/models discovery and /v1/messages proxying, or connect Claude Code to a local shunt instance.
---

# Run shunt

`shunt` is a Rust ([axum](https://github.com/tokio-rs/axum)) HTTP server — a Claude
Code LLM gateway. It has **no GUI**: you drive it with `curl`. It listens on
`127.0.0.1:3001` by default and serves an Anthropic-Messages surface
(`GET /v1/models`, `POST /v1/messages`, `POST /v1/messages/count_tokens`, `HEAD /`).
For each mapped `model` id it diverts inference to another provider (OpenAI / Codex /
ChatGPT via the OpenAI Responses API); everything else passes through to Anthropic.

**Primary agent path:** run the committed driver
[`.claude/skills/run-shunt/smoke.sh`](smoke.sh). It builds the binary, spins up a
local mock upstream (so no real API key is needed), launches the gateway, and drives
every route end to end with assertions. That is the way to confirm a change works.

All paths below are relative to the repo root (the `shunt/` directory).

## Prerequisites

Everything the driver needs is already standard on macOS/Linux dev boxes:

```bash
cargo --version     # Rust stable (built with 1.94); toolchain via rust-toolchain.toml if present
python3 --version   # smoke.sh uses http.server as a stand-in upstream
curl --version
jq --version        # smoke.sh asserts JSON responses with jq
```

On a bare Ubuntu container: `apt-get install -y curl jq python3` and install Rust via
`rustup` if `cargo` is missing.

## Build

```bash
cargo build          # debug binary at target/debug/shunt
```

## Run (agent path) — the smoke driver

This is what you run to see shunt working. It is hermetic (no network, no
credentials) and exits non-zero on the first failed assertion.

```bash
.claude/skills/run-shunt/smoke.sh
```

Expected tail:

```
  PASS shunt check -> config ok
  PASS HEAD / -> 200 (server live)
  PASS GET /v1/models returns configured model
  PASS POST /v1/messages proxied to upstream and returned its body
  PASS malformed request -> 400 invalid_request_error
All smoke checks passed.
```

What it covers: config validation (`shunt check`), liveness (`HEAD /`), model
discovery (`GET /v1/models`), the full proxy forward path (`POST /v1/messages`
routed to a local mock that stands in for `api.anthropic.com`), and the routing
error path (a body with no `model` field → `400 invalid_request_error`). Override
`SHUNT_PORT` / `MOCK_PORT` if `31711`/`31712` are taken.

### Drive it by hand

Write a config, launch, and curl it yourself:

```bash
./target/debug/shunt run --config ./shunt.toml    # or copy shunt.toml.example first
```

Then, against the running server:

```bash
curl -s "http://127.0.0.1:3001/v1/models?limit=1000" | jq .
# => {"data":[{"id":"claude-opus-via-codex","display_name":"Opus (via Codex)"}]}
```

Validate a config without starting the server:

```bash
./target/debug/shunt check --config ./shunt.toml   # prints "config ok" or a precise error
```

CLI shape: `shunt run|check [--config <path>]` (also `shunt --check`). Default config
path is `./shunt.toml`; `SHUNT_`-prefixed env vars override (with `__` for nesting).

## Direct invocation (internal logic — most PRs touch this)

The interesting code is the **Anthropic Messages ⇄ OpenAI Responses translation**
(`src/adapters/`, `src/model/`) and routing (`src/routing.rs`). These are covered by
unit + integration tests — the fastest inner loop for a PR touching them:

```bash
cargo test --workspace                              # all tests (9 pass as of this writing)
cargo test --test responses_translate               # the translation integration suite
```

Full pre-PR gate (matches CI):

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --workspace
```

## Run (human path) — connect Claude Code to a local shunt

Point Claude Code at the running gateway. shunt does **not** validate the credential,
but Claude Code still needs one set or it drops to its login wizard. Per the
[gateway-connect docs](https://code.claude.com/docs/en/llm-gateway-connect):

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:3001
export ANTHROPIC_AUTH_TOKEN=local-dummy        # any string; shunt ignores it
export CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1   # opt-in: pull /v1/models into the picker
export ANTHROPIC_CUSTOM_MODEL_OPTION=gpt-5.2-codex    # add a non-claude id to /model (see Gotchas)
claude    # started from the same shell; /status shows the base URL
```

Verify the wiring without opening Claude Code (this is the docs' own check):

```bash
curl -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
  -H "anthropic-version: 2023-06-01" -H "content-type: application/json" \
  -d '{"model":"claude-opus-via-codex","max_tokens":1,"messages":[{"role":"user","content":"."}]}'
```

With a default (anthropic-routed) config this forwards to `api.anthropic.com` and
needs a real Anthropic key in the header to get a `200`; a mapped model routed to
`openai`/`codex` needs that provider's credential instead. For a credential-free run,
use `smoke.sh` (mock upstream) rather than this path.

## Gotchas

- **No GUI, no `npm start`.** It's a Rust HTTP server. "Running it" means launch +
  `curl`. The driver is the smoke script.
- **Model discovery drops non-`claude`/`anthropic` ids.** Claude Code's
  `/v1/models` importer ignores any `id` not starting with `claude` or `anthropic`,
  so to route to e.g. `gpt-5.2-codex` you either alias it under a `claude…` discovery
  id **or** add it via `ANTHROPIC_CUSTOM_MODEL_OPTION` (the primary way). This is a
  Claude Code constraint, not a shunt bug.
- **shunt ignores the request credential.** `GET /v1/models` reads
  `authorization`/`x-api-key` but discards it (`src/discovery.rs`), and the proxy just
  forwards headers upstream. So any dummy token works for local driving — the real key
  only matters to the *upstream* provider.
- **`shunt check` is strict about provider shape.** `providers.openai.adapter` and
  `providers.codex.adapter` must be `"responses"`; `openai.auth` must be `api_key`,
  `codex.auth` must be `chatgpt_oauth` — otherwise `check` fails with a specific error
  (`src/config.rs`). Partial TOML is fine: figment merges your file over built-in
  defaults, so you only need to specify what differs.
- **A discovery `[[models]]` entry with no matching `[[routes]]`** logs a `WARN` at
  startup/`check` but is not fatal.
- **`GET /protocol` is the machine-readable gateway contract.** It is unauthenticated
  and reports shunt's package version, Anthropic-Messages format, supported endpoints,
  header handling, attribution behavior, and model-discovery constraints.
- **zsh quoting:** quote URLs containing `?` (globbing) and mind `noclobber` on `>`
  redirects when driving by hand in this repo's shell.

## Troubleshooting

- **`smoke.sh` fails at "mock upstream did not become ready"** — a stale process holds
  the port. `pkill -f 'target/debug/shunt'` and re-run, or set `SHUNT_PORT`/`MOCK_PORT`.
- **`502 Bad Gateway` / `api_error: error sending request for url (...)`** — shunt
  reached routing but the upstream `base_url` was unreachable (wrong host/port, or the
  mock/provider isn't up). This is the correct error mapping (`src/error.rs`), not a
  crash. Check the target `base_url`.
- **`400 invalid_request_error: request body must include a JSON model field`** — the
  request body isn't JSON with a `model` key. Routing happens before forwarding
  (`src/routing.rs`).
- **`cargo clippy` fails the build** — CI sets `RUSTFLAGS=-D warnings`; warnings are
  errors. Fix them before a PR (`cargo clippy --all-targets --all-features -- -D warnings`).
