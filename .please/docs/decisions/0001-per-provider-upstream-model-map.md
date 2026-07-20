# ADR-0001: Per-provider upstream model map

## Status

Accepted — amended by [ADR-0002](0002-ordered-upstreams-failover.md): the
one-provider restriction is lifted for configurations declared through the
ordered `[[upstreams]]` list, which supplies the explicit failover ordering
this ADR required.

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

In the legacy `[providers.<name>]` form the key names a configured provider;
under ADR-0002's ordered `[[upstreams]]` form it names an upstream (multiple
entries allowed). The value is the model id sent to that destination. A
map-bearing model entry unifies discovery, provider selection, and model-id
translation. It is resolved before `[[routes]]`, `[[route_prefixes]]`, and
`server.default_provider` for the same requested id. Provider-level defaults
such as `effort` continue to apply.

As originally accepted, the map had to contain exactly one provider; ADR-0002
lifts this rule for configurations using ordered `[[upstreams]]`. Empty maps,
multiple entries outside that form, an empty or whitespace-only provider name
or upstream model id, unknown provider/upstream names, a same-id `[[routes]]`
entry, a map-bearing id ending in the client-only `[1m]`/`[1M]` context-window hint, and
duplicate `[[models]]` ids where at least one entry is map-bearing are
configuration errors. Map-bearing ids cannot include that suffix because
clients strip it before model matching, which would make the configured entry
unreachable. Pure map-less duplicate ids retain their previous behavior. The
map shape was retained as the extension point for an ordered cross-provider
failover feature, whose ordering must be explicit rather than inferred from
`ProvidersConfig`.

Map-less `[[models]]` entries preserve the previous behavior and continue through exact routes, prefix routes, and the default provider.

Exact-match `[[routes]]` is soft-deprecated in documentation in favor of a map-bearing `[[models]]` entry. The two capabilities unique to `[[routes]]` do not justify recommending a second exact-routing surface: operators do not need unadvertised aliases, and per-route `effort` is redundant because clients can send `output_config.effort` per request while `[providers.<name>].effort` remains available as the provider default. `[[routes]]` remains supported indefinitely with no code warning, so valid existing configurations are not nagged or forced to migrate. If a niche per-model effort use case emerges, the map value can evolve backward-compatibly from a string to a serde-untagged `{ model, effort }` table.

## Consequences

### Positive

- One entry can declare the model id shown to clients, its provider, and its upstream model id.
- Routing intent is colocated with discovery metadata, reducing configuration drift.
- Documentation has one recommended exact-id routing form: `[[models]]` with `[models.upstream_model]`.
- Existing configurations remain valid and retain their routing behavior.
- The map-shaped schema reserved the extension point that ADR-0002 now realizes
  as ordered cross-provider failover.

### Negative

- As originally accepted, cross-provider failover was not available through
  this map; ADR-0002 lifts this for configurations using ordered
  `[[upstreams]]`.
- A model id cannot be declared simultaneously in a map-bearing `[[models]]` entry and `[[routes]]`; operators must choose one exact-routing surface.
- Operators using exact-match `[[routes]]` see a legacy label in documentation, although no migration or removal is planned.
- Validation adds startup failures for malformed map-bearing entries that would otherwise have fallen through to existing routing rules.

### Neutral

- Discovery responses remain unchanged by this decision: entries expose `type`,
  `id`, and optional `display_name` within the standard list envelope.
- Existing map-less discovery entries without an exact route continue to emit a warning.
- `[[routes]]`, `[[route_prefixes]]`, and `server.default_provider` retain their runtime behavior; only exact-match `[[routes]]` is soft-deprecated in documentation.

## Alternatives Considered

- **Keep discovery and routing separate:** Rejected because it preserves duplicated ids and the warning-only linkage that issue #216 is intended to remove.
- **Option B — explicit per-model provider order:** Rejected at the time because shunt then had no cross-provider failover runtime semantics. Introducing an ordered provider list before that behavior existed would have added configuration surface without an implementable contract. The selected map reserved a compatible hook while enforcing one provider until ordered failover was designed; ADR-0002 now supplies that design.
- **Use scalar `provider` and `upstream_model` fields:** Rejected because it diverges from the reference gateway schema and provides no direct extension point for future per-provider mappings.
