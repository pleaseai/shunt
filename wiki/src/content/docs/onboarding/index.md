---
title: "Onboarding"
description: "Audience-specific entry points for understanding and working on shunt."
---

`shunt` is a local gateway for Claude Code: run it, point Claude Code at it with `ANTHROPIC_BASE_URL`, and it diverts only configured model IDs while preserving the Claude Code harness, tools, and skills [README.md:4-20](https://github.com/chatbot-pf/shunt/blob/main/README.md#L4-L20) [docs/running.md:6-9](https://github.com/chatbot-pf/shunt/blob/main/docs/running.md#L6-L9).

| Guide | Audience | What You'll Learn | Time |
|---|---|---|---|
| [Contributor Guide](./contributor-guide.md) | New contributors with Python/JS experience | Rust setup, first PR, gateway code paths, testing | ~30 min |
| [Staff Engineer Guide](./staff-engineer-guide.md) | Staff/principal engineers | Architecture, invariants, tradeoffs, failure modes | ~45 min |
| [Executive Guide](./executive-guide.md) | VP/director-level engineering leaders | Capabilities, risks, ownership, investment thesis | ~20 min |
| [Product Manager Guide](./product-manager-guide.md) | Product managers and stakeholders | User journeys, product capabilities, constraints, FAQ | ~20 min |

```mermaid
flowchart TB
    New[New reader] --> Choice{What do you need?}
    Choice -->|Contribute code| CG[Contributor Guide]
    Choice -->|Evaluate architecture| SG[Staff Engineer Guide]
    Choice -->|Plan investment| EG[Executive Guide]
    Choice -->|Understand user value| PM[Product Manager Guide]
    classDef dark fill:#2d333b,stroke:#6d5dfc,color:#e6edf3;
    class New,Choice,CG,SG,EG,PM dark;
    linkStyle default stroke:#8b949e;
```
<!-- Sources: README.md:4, docs/running.md:1, docs/implementation-plan.md:6 -->
