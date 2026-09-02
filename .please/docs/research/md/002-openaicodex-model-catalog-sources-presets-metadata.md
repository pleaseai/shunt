---
id: 002
title: "openai/codex: model catalog sources, presets, metadata, and runtime fetching"
url: "https://github.com/openai/codex/tree/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs"
date: 2026-07-18
summary: "Codex uses a mixed catalog: bundled models.json seeds startup, authenticated OpenAI-compatible providers can refresh it from GET /models, and model_catalog_json selects a fully static catalog. ModelPreset is derived from richer ModelInfo metadata; unknown configured slugs remain accepted through fallback metadata."
tags: [openai-codex, rust, models, model-catalog, config, api, reference]
---

# openai/codex: model catalog sources, presets, metadata, and runtime fetching

## Version examined

`openai/codex` `main` at commit [`5c0e582c59892dbec89af78ae62c784d3da6c9cb`](https://github.com/openai/codex/commit/5c0e582c59892dbec89af78ae62c784d3da6c9cb), committed 2026-07-18.

## Direct answer

Codex uses a **mixed model catalog**:

1. A bundled static catalog, [`codex-rs/models-manager/models.json`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/models.json#L1-L68), is compiled into the binary by `bundled_models_response()` with `include_str!` ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/lib.rs#L11-L15)).
2. `OpenAiModelsManager` seeds itself from that bundled catalog, then may refresh from an authenticated provider's OpenAI-compatible `GET <provider-base>/models?client_version=...` endpoint and cache the response in `models_cache.json` ([manager](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L229-L245), [fetch path](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L334-L386), [HTTP client](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/codex-api/src/endpoint/models.rs#L30-L77)).
3. If `model_catalog_json` is configured, Codex parses that file into `ModelsResponse` and constructs `StaticModelsManager`; this bypasses remote refresh for the process ([config loader](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/core/src/config/mod.rs#L1873-L1902), [manager choice](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/model-provider/src/provider.rs#L313-L335)).

Thus, it is not a generic discovery call to the public OpenAI `GET /v1/models` followed by inference from sparse IDs. It calls the active provider's catalog endpoint, conventionally `/models` relative to its configured base, and expects Codex-specific rich `ModelInfo` objects ([wire struct](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/protocol/src/openai_models.rs#L352-L434)). For ChatGPT auth, the default base is `https://chatgpt.com/backend-api/codex`, so the effective endpoint is `https://chatgpt.com/backend-api/codex/models?client_version=...`; API-key auth defaults to `https://api.openai.com/v1`, yielding `/v1/models?client_version=...` ([base selection](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/model-provider-info/src/lib.rs#L241-L277)).

## Key files

| Path | Role |
|---|---|
| [`codex-rs/models-manager/models.json`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/models.json#L1-L68) | Bundled full `ModelInfo` catalog compiled into Codex. |
| [`codex-rs/models-manager/src/lib.rs`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/lib.rs#L11-L15) | Deserializes bundled `models.json` through `bundled_models_response()`. |
| [`codex-rs/models-manager/src/manager.rs`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L79-L205) | `ModelsManager`, dynamic/static implementations, refresh/cache policy, picker construction, model-info lookup. |
| [`codex-rs/models-manager/src/model_presets.rs`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/model_presets.rs#L1-L5) | Explicitly records that hardcoded model presets were removed. |
| [`codex-rs/protocol/src/openai_models.rs`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/protocol/src/openai_models.rs#L200-L240) | Defines picker-facing `ModelPreset`, backend/catalog `ModelInfo`, and `ModelInfo -> ModelPreset`. |
| [`codex-rs/model-provider/src/models_endpoint.rs`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/model-provider/src/models_endpoint.rs#L37-L145) | Concrete `ModelsEndpointClient` implementation, auth/provider resolution, timeout, request dispatch. |
| [`codex-rs/codex-api/src/endpoint/models.rs`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/codex-api/src/endpoint/models.rs#L30-L77) | Builds `GET models?client_version=...`, parses `ModelsResponse`, reads ETag. |
| [`codex-rs/core/src/config/mod.rs`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/core/src/config/mod.rs#L613-L647) | Holds selected `Config.model`, model overrides, and loads optional `model_catalog_json`. |
| [`codex-rs/core/src/session/mod.rs`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/core/src/session/mod.rs#L563-L605) | Refreshes catalog, resolves selected/default model, and obtains full metadata. |
| [`codex-rs/tui/src/chatwidget/model_popups.rs`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/tui/src/chatwidget/model_popups.rs#L9-L31) | `/model` picker reads the already-populated in-memory catalog. |

## Presets and current bundled model IDs

There is no current `builtin_model_presets()` model function. The source states directly:

```rust
/// Hardcoded model presets were removed; model listings are now derived from the active catalog.
```

([`model_presets.rs`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/model_presets.rs#L1-L5))

At the pinned commit, bundled `models.json` contains these eight entries:

| Model slug | Display name | Picker | API | Priority | Default reasoning | Context / max context |
|---|---|---:|---:|---:|---|---:|
| `gpt-5.6-sol` | GPT-5.6-Sol | yes | yes | 1 | low | 372,000 / 372,000 |
| `gpt-5.6-terra` | GPT-5.6-Terra | yes | yes | 2 | medium | 372,000 / 372,000 |
| `gpt-5.6-luna` | GPT-5.6-Luna | yes | yes | 3 | medium | 372,000 / 372,000 |
| `gpt-5.5` | GPT-5.5 | yes | yes | 7 | medium | 272,000 / 272,000 |
| `gpt-5.4` | GPT-5.4 | yes | yes | 16 | medium | 272,000 / 1,000,000 |
| `gpt-5.4-mini` | GPT-5.4-Mini | yes | yes | 23 | medium | 272,000 / 272,000 |
| `gpt-5.2` | GPT-5.2 | yes | yes | 29 | medium | 272,000 / 272,000 |
| `codex-auto-review` | Codex Auto Review | hidden | yes | 43 | medium | 272,000 / 1,000,000 |

Evidence: first entry and fields ([lines 3-68](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/models.json#L3-L68)); remaining slugs appear at [`gpt-5.6-terra`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/models.json#L118-L180), [`gpt-5.6-luna`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/models.json#L230-L288), [`gpt-5.5`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/models.json#L338-L394), [`gpt-5.4`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/models.json#L444-L498), [`gpt-5.4-mini`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/models.json#L545-L599), [`gpt-5.2`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/models.json#L641-L695), and [`codex-auto-review`](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/models.json#L737-L791).

This is version-specific. Older names such as `gpt-5-codex` or `o3` are not in this pinned bundled catalog; a backend response or custom catalog can still supply other models because the active catalog is data-driven.

### `ModelPreset` fields

Picker-facing `ModelPreset` carries: stable `id`, model slug, display name, description, default and supported reasoning efforts, personality support, speed/service tiers and default tier, default marker, upgrade/availability notices, picker visibility, API support, and input modalities ([definition](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/protocol/src/openai_models.rs#L200-L240)). It does **not** carry context-window or output-token limits; those remain in `ModelInfo`.

`ModelPreset` is mechanically derived from `ModelInfo`:

```rust
impl From<ModelInfo> for ModelPreset {
    fn from(info: ModelInfo) -> Self {
        ModelPreset {
            id: info.slug.clone(),
            model: info.slug.clone(),
            display_name: info.display_name,
            // ...
            show_in_picker: info.visibility == ModelVisibility::List,
            // ...
        }
    }
}
```

([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/protocol/src/openai_models.rs#L568-L599))

The manager sorts `ModelInfo` by `priority`, converts each to `ModelPreset`, filters by auth/API support, and marks the first picker-visible preset as default ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L121-L134), [default rule](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/protocol/src/openai_models.rs#L629-L651)).

## Model metadata and ID lookup

There is no current `model_family.rs` or separate family table in this path. Rich metadata is represented directly by `ModelInfo`, including slug/display/description, reasoning options, shell/tool behavior, visibility/API support/priority, service tiers, instructions/personality messages, verbosity, web search/apply-patch/parallel-tool capabilities, truncation policy, input modalities, `context_window`, `max_context_window`, auto-compaction, and other feature flags ([definition](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/protocol/src/openai_models.rs#L352-L434)).

The current `ModelInfo` schema has **no `max_output_tokens` field**. Output headroom is represented indirectly by effective context-window handling and truncation/auto-compaction fields, not a per-model max-output-token value in this catalog type ([context methods](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/protocol/src/openai_models.rs#L436-L452)).

`get_model_info()` obtains the current in-memory catalog and delegates to `construct_model_info_from_candidates()` ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L181-L195)). Lookup uses the longest catalog-slug prefix, so a snapshot-like ID beginning with a known slug inherits that entry. It also supports one simple provider namespace such as `custom/gpt-5.3-codex`. It preserves the exact requested ID in the returned `slug`; if no candidate matches, it builds fallback metadata ([lookup](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L554-L610)).

```rust
let remote = find_model_by_longest_prefix(model, candidates)
    .or_else(|| find_model_by_namespaced_suffix(model, candidates));
let model_info = if let Some(remote) = remote {
    ModelInfo { slug: model.to_string(), ..remote }
} else {
    model_info::model_info_from_slug(model)
};
```

The unknown-model fallback uses the bundled base prompt, defaults to a 272,000-token context/max context, and disables most advanced capabilities ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/model_info.rs#L74-L118)). Config overrides are then applied; notably, `model_context_window` is clamped by `max_context_window` ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/model_info.rs#L22-L61)).

## Dynamic fetching and picker behavior

`RefreshStrategy` is explicit: `Online` always fetches, `Offline` only considers cache/current state, and `OnlineIfUncached` uses a fresh cache first ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L50-L69)). Root sessions use `OnlineIfUncached`; non-root agents use `Offline` ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/core/src/session/mod.rs#L563-L578)). The manager only attempts remote refresh when the endpoint uses Codex-backend auth or has command auth ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L389-L391)).

The HTTP request path is exactly `models`, method GET, with `client_version=<whole semver>` appended. It parses `ModelsResponse` and captures the response ETag ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/codex-api/src/endpoint/models.rs#L30-L77)). Provider auth is attached by the endpoint session/auth provider, and provider-defined headers include a Codex version header plus optional OpenAI organization/project environment headers ([provider defaults](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/model-provider-info/src/lib.rs#L329-L363)).

For ChatGPT auth, a non-empty remote response containing at least one picker-visible model becomes the source of truth. Otherwise, fetched entries replace matching bundled slugs and append new slugs to the bundled catalog ([merge policy](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L397-L427)). Fetches are cached to `$CODEX_HOME/models_cache.json` for 300 seconds and tied to the whole client version ([manager constants](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L25-L26), [cache format](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/cache.rs#L159-L168)).

The TUI `/model` popup does not issue HTTP itself. Startup has already populated a `ModelCatalog`; the popup calls `try_list_models()` and filters `show_in_picker` ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/tui/src/chatwidget/model_popups.rs#L9-L31), [filter](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/tui/src/chatwidget/model_popups.rs#L73-L77)).

## Config integration

`Config.model` is an optional string override, not a foreign key that must validate against a hardcoded enum ([definition](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/core/src/config/mod.rs#L613-L647)). At session startup:

1. The catalog is refreshed/loaded.
2. `get_default_model(&config.model, ...)` resolves the effective model.
3. `get_model_info(effective_model, ...)` maps that exact model ID to catalog metadata or fallback metadata.

([session path](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/core/src/session/mod.rs#L563-L605))

For the ordinary OpenAI-compatible manager, an explicit `config.model` is returned unchanged; absent one, the marked catalog default is selected ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L149-L179)). `StaticModelsManager` can optionally replace an unavailable requested model with its catalog default when provider fallback is allowed ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/models-manager/src/manager.rs#L456-L490)). Therefore arbitrary model IDs are generally not rejected solely because they are absent from the picker catalog; they proceed with fallback metadata unless a provider-specific fallback policy substitutes the default.

`model_catalog_json` is startup-only and must contain a non-empty serialized `ModelsResponse` ([schema](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/core/config.schema.json#L5256-L5263)). Its presence switches the configured provider to `StaticModelsManager`; otherwise Codex uses `OpenAiModelsManager` ([source](https://github.com/openai/codex/blob/5c0e582c59892dbec89af78ae62c784d3da6c9cb/codex-rs/model-provider/src/provider.rs#L313-L335)).

## Conclusion: static vs. dynamic

**Explicit classification: mixed.** Codex ships a static rich catalog, dynamically fetches a replacement/overlay catalog from the active provider's `/models` endpoint when eligible, caches it, and optionally accepts a fully static user catalog. Picker presets are derived views of the active rich catalog, not a separate current hardcoded Rust list. Unknown explicitly configured IDs are not catalog-validated in the normal OpenAI-compatible path; they receive fallback metadata.
