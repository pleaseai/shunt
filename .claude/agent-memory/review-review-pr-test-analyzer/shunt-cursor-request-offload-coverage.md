---
name: shunt-cursor-request-offload-coverage
description: PR #272 request-framing and image-decode offload test review, including shared-state isolation and remaining behavioral gaps
metadata:
  type: project
---

PR #272's boundary arithmetic is correct: decoded payloads of 65,534 and 65,535 bytes estimate at 65,535 and stay inline, while 65,536 estimates at 65,538 and offloads. Every changed test that acquires the process-wide CPU permit or observes offload globals holds the shared observer mutex; 10 parallel Cursor-suite stress iterations passed. Existing image and gzip assertions were preserved rather than weakened.

Coverage gaps remain around the production `open_turn` framing-offload wiring, both call-site join-error mappings, aggregate multi-image threshold selection, and a deterministic mixed-workload proof of the shared concurrency bound.

**Why:** The current direct helper tests can remain green if production framing returns to the async worker, while isolated one-sided concurrency tests do not fully guard the aggregate admission contract.

**How to apply:** For future CPU-offload PRs, test the production wiring rather than a test-only replica, inject failures at each caller boundary, include cumulative-input cases, and use gated tasks for deterministic shared-cap assertions.
