---
id: 001
title: "openai/codex: how model_verbosity works end-to-end (config -> Responses API)"
url: "https://github.com/openai/codex/blob/d7ba5ff9553a6aa0898a8e3bd5cb3bc00d0c9ddf/codex-rs/core/src/client.rs#L880"
date: 2026-07-14
summary: "Source-grounded trace of Codex CLI's model_verbosity config setting: the Verbosity enum, its per-model gating (support_verbosity/default_verbosity), and how it serializes to the Responses API as text.verbosity."
tags: [openai-codex, rust, responses-api, config, gpt-5, verbosity, reference]
---

# openai/codex: `model_verbosity` implementation trace

SHA used for all permalinks: `d7ba5ff9553a6aa0898a8e3bd5cb3bc00d0c9ddf` (openai/codex `main`, captured 2026-07-14).

## 1. Config type

`Verbosity` enum — `codex-rs/protocol/src/config_types.rs#L74-L79`, lowercase-serialized, default `Medium`:

```rust
pub enum Verbosity {
    Low,
    #[default]
    Medium,
    High,
}
```

Field `model_verbosity: Option<Verbosity>` exists at three layers:
- `ConfigToml` (`codex-rs/config/src/config_toml.rs`) — top-level `config.toml` key.
- `ProfileToml.model_verbosity` (`codex-rs/config/src/profile_toml.rs#L33`) — per-`[profiles.X]` override.
- Resolved `Config` (`codex-rs/core/src/config/mod.rs#L958`), doc comment: *"Optional verbosity control for GPT-5 models (Responses API `text.verbosity`)."*

Also exposed via app-server protocol (`codex-rs/app-server-protocol/src/protocol/v1.rs#L208`, TS schema `v2/Config.ts`) for session/thread-level overrides.

## 2. Threading into the outbound request

`Config.model_verbosity` -> `ModelClient` state (`client.rs#L200`, `#L419`) -> `ModelClient::build_responses_request` (`codex-rs/core/src/client.rs#L880-L895`):

```rust
let verbosity = if model_info.support_verbosity {
    self.state.model_verbosity.or(model_info.default_verbosity)
} else {
    if self.state.model_verbosity.is_some() {
        warn!("model_verbosity is set but ignored as the model does not support verbosity: {}", model_info.slug);
    }
    None
};
let text = create_text_param_for_request(
    verbosity,
    &prompt.output_schema,
    prompt.output_schema_strict,
);
```

`create_text_param_for_request` (`codex-rs/codex-api/src/common.rs#L325-L343`) builds `Option<TextControls>`:

```rust
Some(TextControls {
    verbosity: verbosity.map(std::convert::Into::into),
    format: output_schema.as_ref().map(|schema| TextFormat { .. }),
})
```

`TextControls` (`common.rs#L189-L194`) is the `text` field of `ResponsesApiRequest` (`common.rs#L216/236/288`). Wire enum `OpenAiVerbosity` (`common.rs#L196-L203`) is `#[serde(rename_all = "lowercase")]` with `From<VerbosityConfig>`.

**Exact wire shape:** `{"text": {"verbosity": "low"|"medium"|"high", "format": ...}}` — nested under `text.verbosity`, not a top-level `verbosity` field.

## 3. Unset (`None`) behavior — confirmed by code, not assumed

- If unset and the model supports verbosity, Codex falls back to `model_info.default_verbosity` via `.or(...)` — i.e. **Codex injects the model catalog's own default**, it is not merely omitted from the wire.
- `text` is omitted entirely only when both `verbosity` is `None` *and* there's no `output_schema` (nothing to send).
- If the model does **not** support verbosity but the user configured `model_verbosity` anyway, the value is silently dropped with a `warn!` log — never sent, no substitute default.

## 4. Model gating

Gating fields `ModelInfo.support_verbosity: bool` / `default_verbosity: Option<Verbosity>` defined in `codex-rs/protocol/src/openai_models.rs#L381-L382`. Populated per-model in `codex-rs/models-manager/models.json`. As of this capture, `support_verbosity: true` models: `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.2`, `codex-auto-review` — all GPT-5-family, each with its own `default_verbosity` (mostly `"low"`, one `"medium"`). The unknown/fallback model default (`model_info.rs#L96-L99`) is `support_verbosity: false, default_verbosity: None`.

## 5. Effect of low/medium/high

Upstream OpenAI Responses API behavior (not implemented in codex itself — codex only threads the value through): `low` biases toward terse output, `high` toward longer/more elaborated output, `medium` is balanced default.

## 6. Interactions

- `model_reasoning_effort` and `model_verbosity` are independent sibling `Option` fields at every config layer; built into the request separately (`reasoning` via `Self::build_reasoning`, `text.verbosity` via `create_text_param_for_request`) side by side at `client.rs#L874-L895` — no coupling found.
- Per-profile override confirmed via `ProfileToml.model_verbosity` (`profile_toml.rs#L33`).
- **No runtime `/verbosity` TUI slash command exists** in `codex-rs/tui` as of this SHA (checked `slash_command.rs` and all `*slash*` files) — it is config/profile/session-param only.

## Key files

- `codex-rs/protocol/src/config_types.rs` — `Verbosity` enum
- `codex-rs/protocol/src/openai_models.rs` — `ModelInfo.support_verbosity`/`default_verbosity`
- `codex-rs/config/src/config_toml.rs`, `codex-rs/config/src/profile_toml.rs` — config/profile fields
- `codex-rs/core/src/config/mod.rs` — resolved `Config` field
- `codex-rs/core/src/client.rs` — `build_responses_request` verbosity resolution (~L860-L900)
- `codex-rs/codex-api/src/common.rs` — `TextControls`, `OpenAiVerbosity`, `ResponsesApiRequest.text`, `create_text_param_for_request`
- `codex-rs/core/config.schema.json` — user-facing schema/doc string
- `codex-rs/models-manager/models.json` — per-model verbosity support/defaults
