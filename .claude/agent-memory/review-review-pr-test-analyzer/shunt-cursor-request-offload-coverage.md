---
name: shunt-cursor-request-offload-coverage
description: PR #272 request-framing and image-decode offload test review, including shared-state isolation and remaining behavioral gaps
metadata:
  type: project
---

PR #272's boundary arithmetic is correct: decoded payloads of 65,534 and 65,535 bytes estimate at 65,535 and stay inline, while 65,536 estimates at 65,538 and offloads. Commit `b73f31a` also added a valid aggregate fixture: three 21,841-byte images estimate to 65,529 (inline), while three 21,846-byte images estimate to 65,538 (offloaded), so first-only/max regressions fail. The new frame-0 markers uniquely exercise image bytes/MIME and tool description/schema.

Every Cursor unit test that acquires a request-prep/gzip permit or observes `#[cfg(test)]` offload state now holds `OFFLOAD_OBSERVER`; integration-test binaries cannot race those process-local globals. The cancellation test is deterministic under that lock and survived 25 stress iterations.

Commit `8af7ec4` closed two of the four gaps found in review. The capacity test no longer uses `std::sync::Barrier`: each closure is gated on its own `std::sync::mpsc` channel, so unwinding on a failed assertion drops the senders, every `recv()` returns `Err`, and the unabortable closures exit instead of parking forever and hanging Tokio teardown — verified by injecting a panic mid-test and confirming the binary terminates with the assertion rather than timing out. `gzip_and_request_prep_admit_independently` now saturates each admission class and proves the other still admits work; it is mutation-proven (pointing `spawn_bounded_gzip` at `request_prep_slots()` makes it fail in 5s, exit 101).

Two gaps remain, both **deliberately accepted** rather than overlooked: the framing-marker test calls the async helper directly rather than the production `forward` call site, and the failure test calls `request_prep_error` directly rather than inducing join errors through either `.map_err` call site. Closing either requires a test seam in production code — `forward` cannot be driven by wiremock because `SHUNT_CURSOR_AGENT_BASE_URL` is validated to an https `cursor.sh` host and cached in a `OnceLock`, and forcing a join error means panic-injecting into a production closure or closing a process-wide static semaphore that every other test shares. Marker resets (`reset_request_prep_path()`) were added before each asserted action so stale test-order state cannot mask a regression.

**Why:** Direct helper assertions remain green if production bypasses the helper or its call-site error classification regresses. Independently sampling each semaphore does not prove the wrappers use distinct admission classes. A non-cancellation-safe blocking barrier turns rare scheduling/interference failures into stuck CI.

**How to apply:** Reset test markers before the asserted action; saturate one admission class while proving the other enters; use a release gate whose drop path wakes blocked closures, never a `Barrier`, around `spawn_blocking`. Prove any concurrency-bound test non-vacuous by mutating the bound and confirming the test *fails* rather than hangs — a hang (exit 124) is itself a defect in the test. When a coverage gap needs a production test seam, weigh that against the PR's scope instead of assuming it must be closed.
