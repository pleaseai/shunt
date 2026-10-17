# Tech Debt Tracker

> Tracked across all tracks. Updated during implementation and retrospectives.

## Active

| ID | Source Track | Description | Priority | Created |
|----|------------|-------------|----------|---------|
| TD-002 | per-window-max-utilization-20261004 | `/usage` and admin snapshot evaluate caps per alias row while selection uses the identity representative; can disagree for aliases with different caps | P3 | 2026-10-04 |
| TD-003 | pluggable-store-20261001 | A reload that changes `[server.gateway].state_path` silently redirects `save_if_dirty` (it reads `state.refreshed().config`) with no restart-only warning; pre-existing | P3 | 2026-10-17 |
| TD-004 | pluggable-store-20261001 | `docs/dashboard-metrics.md` series count (18, "sixteen of eighteen write-only") is stale against `src/metrics.rs`; needs a recount | P3 | 2026-10-17 |

## Resolved

| ID | Source Track | Description | Resolved In | Date |
|----|------------|-------------|-------------|------|
| TD-001 | per-window-max-utilization-20261004 | Cap verdict re-checked after selection under a second lock (microsecond race, duplicated selectability rules); verdict now returned from the selection pass | #739 | 2026-10-05 |
