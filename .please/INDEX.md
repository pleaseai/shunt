# .please/ Workspace Index

> Central navigation for all project artifacts managed by the please plugin.

## Project Documents

| Document | Purpose |
|---|---|
| [`../AGENTS.md`](../AGENTS.md) | Agent build/test/style/boundary instructions (read first) |
| [`../README.md`](../README.md) | Project overview, quickstart, supported providers/models |
| [`../CLAUDE.md`](../CLAUDE.md) | Project-level AI instructions |

## Directory Map

| Path | Purpose |
|---|---|
| `state/` | Runtime session state (progress) — not tracked in git |
| `docs/tracks/` | Implementation tracks (spec + plan) → [Tracks](docs/tracks.jsonl) |
| `docs/product-specs/` | Product-level specifications → [Product Specs Index](docs/product-specs/index.md) |
| `docs/decisions/` | Architecture Decision Records → [Decisions Index](docs/decisions/index.md) |
| `docs/investigations/` | Bug investigation reports |
| `docs/research/` | Research documents |
| `docs/references/` | External reference materials (-llms.txt etc.) |
| `docs/knowledge/` | Stable project context (product, tech-stack, guidelines) |
| `docs/brainstorms/` | Brainstorm requirements documents (`$PLEASE_DOCS_BRAINSTORMS`) |
| `docs/ideation/` | Ideation candidate documents (`$PLEASE_DOCS_IDEATION`) |
| `templates/` | Workflow templates (plugin-provided) |
| `scripts/` | Utility scripts (plugin-provided) |

## Configuration

See [config.yml](config.yml) for workspace settings.

## Workflows

- `/please:new-track` — Create feature specification and architecture plan
- `/please:implement` — TDD implementation from plan file
- `/please:finalize` — Finalize PR, move track to completed
