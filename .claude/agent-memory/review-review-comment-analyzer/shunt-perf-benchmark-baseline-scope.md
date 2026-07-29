---
name: shunt-perf-benchmark-baseline-scope

description: The perf_issues module mixes frozen pre-#261 baselines with later issue-specific benchmark pairs, so its module docs must scope each comparison explicitly.
metadata:
  type: project
---

`benches/perf_issues.rs` contains frozen `pre_parse_once_*` baselines for the parse-once work that landed in PR #261, while later performance issues can add independent before/after pairs to the same module (PR #273 / issue #265 added clone-vs-borrow benches).

**Why:** A blanket statement that all `pre_*` functions are “pre-refactor” becomes ambiguous as the file accumulates later refactors and can make readers treat a pre-#261 baseline as a pre-#265 baseline.

**How to apply:** Whenever benchmarks are added to this module, make the module doc name the issue/refactor associated with each frozen pair rather than describing one global “pre-refactor” era.
