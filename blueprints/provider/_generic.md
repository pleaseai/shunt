# shunt blueprint: implement a new provider

You are a coding agent implementing support for a genuinely new provider protocol in the shunt codebase. Follow these steps; verify each before moving on.

Research starting point: {{RESEARCH_URL}}

## Establish the protocol contract

Read the research starting point and authoritative linked material. Capture request and response schemas, authentication, model identifiers, endpoint construction, error shapes and statuses, streaming framing, tool calls and tool results, reasoning content, images, usage accounting, cancellation, retries, and token counting. Build small redacted wire fixtures or local mocks where documentation is ambiguous. Never use or commit a real credential.

## Decide whether code is necessary

First test whether an existing adapter kind already fits:

- `anthropic`: the provider speaks the Anthropic Messages API and SSE format.
- `responses`: the provider speaks the OpenAI Responses API and its streaming events.
- `cursor`: the provider speaks the existing Cursor ConnectRPC/protobuf protocol.

If one fits, do not create a new adapter. Add a table-driven preset/config entry with explicit `kind`, `base_url`, and auth defaults, then document and test it. A provider offering only OpenAI Chat Completions does not automatically fit the Responses adapter.

Only implement a new adapter when wire evidence proves that none of these protocols can represent the provider faithfully.

## Read the repository before editing

Read `AGENTS.md`, `CONTRIBUTING.md`, and the relevant architecture notes. Trace the current paths:

- `src/adapters/` owns upstream protocol dispatch and streaming responses.
- `src/model/` owns Anthropic Messages and OpenAI Responses translation.
- `src/auth/` owns credential lookup, refresh, and origin restrictions.
- `src/config.rs` and its config submodules own schema validation and table-driven presets.
- `src/routing.rs` resolves public model ids to ordered upstream routes.
- `tests/` contains focused protocol and translation integration tests.

Use `docs/m6-xai-provider.md` as a worked provider milestone and `docs/m1-responses-translation.md` as a worked translation milestone. Match surrounding abstractions and naming before adding new ones.

## Design the smallest integration

Write down the protocol-to-Anthropic mapping and unsupported behavior before coding. Prefer extending shared translation primitives over copying an adapter. Add a new auth mode only when existing `passthrough`, `api_key`, or OAuth modes cannot safely express the credential. Enforce HTTPS and allowed origins for injected bearer credentials. Keep provider selection table-driven; do not scatter provider-name conditionals across proxy, routing, or auth code.

If a preset is sufficient, add it to the same static preset table that supplies `kind`, `base_url`, and default auth. Ensure explicit config fields override preset defaults.

## Preserve project boundaries

- Preserve streaming semantics. Never buffer an upstream SSE response unless the client requested non-streaming output.
- Keep gateway-owned errors in the Anthropic error shape, except on the inbound Codex endpoint, where gateway-owned errors use the OpenAI Responses error shape.
- Prefer table-driven configuration over hardcoded provider branching.
- Preserve raw upstream status or failure class wherever routing and failover need it; do not flatten information prematurely.
- Never weaken credential origin checks, inbound auth, or secret handling.
- Never commit secrets, tokens, captured authorization headers, or generated local config files.

## Implement with focused tests

Add unit tests for config parsing, defaults, overrides, validation, and routing. Add focused protocol integration tests under `tests/` with a local mock server for:

- request translation and required headers;
- non-streaming and streaming responses without buffering;
- tool calls/results and reasoning blocks, if supported;
- upstream errors, malformed frames, and transport failures;
- authentication injection and rejection of unsafe origins;
- model routing and any retry or failover classification;
- unsupported features returning explicit, correctly shaped errors.

Do not remove or weaken existing tests. If a current test appears wrong, document the evidence and obtain maintainer confirmation before changing its expectation.

## Validate the complete change

Run all required gates:

```bash
cargo fmt --all
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --workspace
```

Then run `shunt check` against a redacted example config and exercise one non-streaming and one streaming request against a mock or authorized test account. Confirm no secrets appear in logs or fixtures.

## Keep documentation synchronized

In the same pull request, update every affected surface:

- `README.md` for the capability and provider table;
- the relevant `docs/` engineering or milestone note;
- `site/src/content/docs/` configuration, provider, CLI, and troubleshooting pages, including localized counterparts where the repository maintains them;
- example TOML/YAML config where applicable.

Do not hand-edit `wiki/`; it is generated separately. Record compatibility limits, authentication setup, model selection, streaming behavior, and known provider entitlement constraints.

## Completion evidence

Report the protocol evidence, why an existing adapter did or did not fit, files changed, tests added, all command results, and any behavior intentionally left unsupported. Do not claim completion until format, clippy with warnings denied, the full workspace tests, config validation, and live or mock protocol checks all pass.
