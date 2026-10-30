---
name: shunt-build-env
description: How to build/test shunt in the coder workspace (toolchain pin, feature substitute for --all-features)
metadata:
  type: project
---

In this workspace the default rustc cannot build shunt's deps; use `cargo +1.98.1` with
`CARGO_INCREMENTAL=0 RUSTFLAGS="-D warnings"`. `--all-features` is unbuildable (no ui/dist,
prefill-router needs Python), so substitute `--features bench`.

**Why:** orchestrator brief for the chatgpt-plan track (2026-10-30); disk space is tight (~90% full).
**How to apply:** always pass the same RUSTFLAGS — running cargo without them triggers a full
rebuild into a second fingerprint set (slow, eats disk). Never `cargo clean` unless the disk is full.
