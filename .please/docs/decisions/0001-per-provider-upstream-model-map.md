# ADR-0001: Per-provider upstream model map

## Status

Accepted

## Date

2026-07-20

## Context

shunt previously configured model discovery and model routing through separate surfaces. A `[[models]]` entry advertised an id through `GET /v1/models`, while `[[routes]]`, `[[route_prefixes]]`, and `server.default_provider` independently selected its provider and optional upstream model id. The only link between a curated discovery entry and an exact route was a config-load warning.

Issue #216 aligns shunt with the reference Claude apps gateway's `models:` schema by allowing a model declaration to carry its provider-specific upstream id. This removes duplicated model ids across discovery and routing config and makes the advertised id's destination explicit at the declaration site.

The natural schema is a provider-to-model map, but shunt's `ProvidersConfig` is an unordered `BTreeMap` for selection purposes and shunt does not implement cross-provider model failover. Accepting multiple entries now would therefore create an undefined or misleading provider order.

## Decision

Add an optional `upstream_model` map to each `[[models]]` entry:

```toml
[[models]]
id = "claude-opus-4-8"
display_name = "Claude Opus 4.8"

[models.upstream_model]
codex = "gpt-5.2"
```

The key names a configured provider and the value is the model id sent to that provider. A map-bearing model entry unifies discovery, provider selection, and model-id translation. It is resolved before `[[routes]]`, `[[route_prefixes]]`, and `server.default_provider` for the same requested id. Provider-level defaults such as `effort` continue to apply.

For now, the map must contain exactly one provider. Empty maps, multiple providers, unknown providers, a same-id `[[routes]]` entry, and duplicate map-bearing model declarations are configuration errors. The map shape is retained as the extension point for a future ordered cross-provider failover feature, which must define ordering explicitly rather than infer it from `ProvidersConfig`.

Map-less `[[models]]` entries preserve the previous behavior and continue through exact routes, prefix routes, and the default provider.

## Consequences

### Positive

- One entry can declare the model id shown to clients, its provider, and its upstream model id.
- Routing intent is colocated with discovery metadata, reducing configuration drift.
- Existing configurations remain valid and retain their routing behavior.
- The map-shaped schema remains compatible with a future provider-failover capability.

### Negative

- Cross-provider failover is not available through this map yet.
- A model id cannot be declared simultaneously in a map-bearing `[[models]]` entry and `[[routes]]`; operators must choose one exact-routing surface.
- Validation adds startup failures for malformed map-bearing entries that would otherwise have fallen through to existing routing rules.

### Neutral

- Discovery responses remain unchanged because `GET /v1/models` still exposes only `id` and optional `display_name`.
- Existing map-less discovery entries without an exact route continue to emit a warning.

## Alternatives Considered

- **Keep discovery and routing separate:** Rejected because it preserves duplicated ids and the warning-only linkage that issue #216 is intended to remove.
- **Option B — explicit per-model provider order:** Rejected for now because shunt has no cross-provider failover runtime semantics. Introducing an ordered provider list before that behavior exists would add configuration surface without an implementable contract. The selected map reserves a compatible hook while enforcing one provider until ordered failover is designed.
- **Use scalar `provider` and `upstream_model` fields:** Rejected because it diverges from the reference gateway schema and provides no direct extension point for future per-provider mappings.
