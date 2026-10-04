# Tech Debt Tracker

> Tracked across all tracks. Updated during implementation and retrospectives.

## Active

| ID | Source Track | Description | Priority | Created |
|----|------------|-------------|----------|---------|
| TD-001 | per-window-max-utilization-20261004 | Cap verdict re-checked after selection under a second lock (microsecond race, duplicated selectability rules); return it from `select_order` — #739 | P3 | 2026-10-04 |
| TD-002 | per-window-max-utilization-20261004 | `/usage` and admin snapshot evaluate caps per alias row while selection uses the identity representative; can disagree for aliases with different caps | P3 | 2026-10-04 |

## Resolved

| ID | Source Track | Description | Resolved In | Date |
|----|------------|-------------|-------------|------|
