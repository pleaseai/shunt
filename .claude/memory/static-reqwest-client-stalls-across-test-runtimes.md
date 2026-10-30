---
name: static-reqwest-client-stalls-across-test-runtimes
description: A process-static reqwest client shared by #[tokio::test] cases stalls tests order-dependently because pooled connections outlive each test's runtime
type: gotcha
source: "track chatgpt-plan-provider-20261030 (PR #871)"
---

# A static reqwest client stalls tests across tokio runtimes

`shared::token_refresh_client()` (`src/auth/shared.rs`) is a process-static reqwest client.
Every `#[tokio::test]` runs on its own runtime, and wiremock reuses server ports between tests.
A later test can check out a pooled keep-alive connection whose driving runtime has already
shut down. That request then hangs until its timeout (10-30 s), so the failures depend on test
order. Adding one unrelated test turned an intermittent flake into a consistent failure.

Evidence: full `--lib` failed 2 tests on 3 of 3 runs (chatgpt-plan
`token_in_refresh_window_is_renewed_and_rotation_persisted`, claude
`refresh_with_a_stalled_response_body_times_out_as_transient`), while `-- auth::` and
`-- --skip auth::` each passed alone.

Fix: `#[cfg(test)] pool_max_idle_per_host(0)` on that client. Full `--lib` then passed (3623
tests, 2 of 2 runs). Production pooling is unchanged.

If a test that uses a shared static HTTP client stalls only when run with the full suite,
suspect connection pooling across runtimes before suspecting the code under test.
