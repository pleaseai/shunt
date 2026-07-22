# `shunt add` blueprint registry

`shunt add` exposes embedded Markdown blueprints for coding agents. Blueprints describe how to configure a shipped upstream preset or research and implement a genuinely new provider protocol. The command is read-only: it does not edit the operator's files, install dependencies, or access the network.

## Registry and embedding

`src/blueprints.rs` owns a static, table-driven registry. Each named entry records its kind, canonical slug, aliases, one-line description, and Markdown body. Bodies live under `blueprints/` and are embedded into the binary at compile time with `include_str!`, so retrieval is deterministic and works offline.

Two kinds are available:

- `upstream`: named guides for shipped presets plus a generic URL-driven compatible-endpoint guide.
- `provider`: a generic URL-driven contributor guide for deciding whether an existing adapter fits and, if necessary, implementing a new protocol adapter.

## Command semantics

```text
shunt add
shunt add <kind>
shunt add <kind> <name-or-url> [--print]
```

A bare command lists both kinds. Supplying a kind lists only that kind. A known slug or alias prints its embedded Markdown. An absolute `http://` or `https://` URL selects the kind's generic template and replaces its `{{RESEARCH_URL}}` marker. Relative paths and unknown names fail with the available slugs and URL form in the error.

Blueprint Markdown always goes to stdout without decoration, including when `--print` is omitted. Without `--print`, an interactive stderr receives one pipe-to-agent hint; redirected stderr and pipeline stdout remain clean. Listing output is also stdout-only.

## Follow-up

The upstream blueprint registry in `src/blueprints.rs` defines its own slugs, one-line descriptions, and Markdown bodies, duplicating the preset identifiers introduced by the ordered `[[upstreams]]` work (see `docs/upstreams-failover.md`). Once both implementations are on `main`, unify the blueprint registry and the config preset table behind one source of truth so a new preset cannot ship without a corresponding registry decision.
