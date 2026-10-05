# Tech Debt Tracker

> Tracked across all tracks. Updated during implementation and retrospectives.

## Active

| ID | Source Track | Description | Priority | Created |
|----|------------|-------------|----------|---------|
| TD-001 | spend-enforcement-20261003 | Spend counters saturate at u64::MAX femto-USD (~$18.4k per principal per window) (#736) | Medium | 2026-10-05 |
| TD-002 | spend-enforcement-20261003 | Inbound Codex endpoint (`/v1/responses`, WebSocket) not spend-enforced or metered (#733) | Medium | 2026-10-05 |
| TD-003 | spend-enforcement-20261003 | Data-only SSE frames (no `event:` line) are not metered | Low | 2026-10-05 |
| TD-004 | spend-enforcement-20261003 | `shunt:anonymous` not reserved against authenticated identities | Low | 2026-10-05 |
| TD-005 | spend-enforcement-20261003 | Caught meter panic leaves the counters mutex poisoned (admission `.expect`) | Low | 2026-10-05 |
| TD-006 | spend-enforcement-20261003 | Stale "injects a credential"/"all-passthrough" doc comments; `restore_from` doc; spend files over 500 lines | Low | 2026-10-05 |

## Resolved

| ID | Source Track | Description | Resolved In | Date |
|----|------------|-------------|-------------|------|
