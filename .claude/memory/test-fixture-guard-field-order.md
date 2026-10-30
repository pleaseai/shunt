---
name: test-fixture-guard-field-order
description: A test fixture holding config::test_env_lock() and EnvVarGuards must declare the guards first, or the lock is released before the vars are restored
type: gotcha
source: "track chatgpt-plan-provider-20261030 (PR #871)"
---

# Declare env guards before the env lock in a test fixture

Rust drops struct fields in declaration order. When one fixture struct holds both the
`config::test_env_lock()` guard and the `EnvVarGuard`s it protects, and the lock comes first,
the lock is released before the env vars are restored. The next test then takes the lock and
sets its own vars, and the old guards' restore clears them under it. The result is
intermittent wrong-account and 502 failures, about 1 run in 5.

Evidence: `-- pool_plan` produced 2 different single failures in 23 runs before the reorder,
and was clean in 25 of 25 runs after it.

Put the guards first, as `src/auth/chatgpt_plan/logout/tests.rs` `Env` does:

```rust
struct Env {
    // Drop order: the env var is restored before the lock is released.
    _env: EnvVarGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
    // ...
}
```

Related: [[static-reqwest-client-stalls-across-test-runtimes]]
