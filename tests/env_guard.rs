//! The behaviour of the shared environment guard itself.
//!
//! Every other test binary depends on `common::EnvVars` doing four things, and
//! nothing in those binaries asserts any of them — they use the guard for their
//! own purposes and would still pass if its restore were subtly wrong, which is
//! how a guard silently stops guarding. These are the four.
//!
//! Each test reads the environment *after* dropping its guard, to prove the
//! restore happened. That read is itself the operation the lock serializes, so
//! it takes the lock again rather than running unguarded — dropping the guard
//! and then reading would be the same reader-side race this guard exists to
//! close.

mod common;

/// Restoring what the test *found* is what makes the guard composable: a test
/// that writes the same name twice must still leave the surrounding value, not
/// its own first write. `remember` is first-touch-wins for that reason, and this
/// is the assertion that fails if it is ever changed to overwrite.
#[test]
fn the_guard_restores_the_value_it_found_not_the_first_one_the_test_wrote() {
    const NAME: &str = "SHUNT_TEST_ENV_GUARD_FIRST_TOUCH";

    let mut vars = common::set_env_blocking(&[(NAME, "first")]);
    vars.set(NAME, "second");
    assert_eq!(
        std::env::var(NAME).as_deref(),
        Ok("second"),
        "the later write is what the test body sees"
    );
    drop(vars);

    // Absent, not "first": the guard saved the pre-test state on the first touch
    // and the second `set` did not overwrite that record.
    let _after = common::set_env_blocking(&[]);
    assert_eq!(
        std::env::var_os(NAME),
        None,
        "both writes restore to the value that was there before the test"
    );
}

/// The other half of the restore, and the one no other test here reaches: when
/// the name already had a value, `Drop` must put *that* value back rather than
/// simply removing the name. Every other test in this file starts from an absent
/// name, so they would all still pass if `Drop` ignored `saved` and always
/// called `remove_var`.
///
/// Seeding a pre-existing value cannot come from the guard — the guard's whole
/// job is to leave nothing behind — so the two writes below are raw, taken under
/// the lock so they are still serialized. `tests/env_lock_coverage.rs` exempts
/// this file for exactly those two.
#[test]
fn the_guard_puts_a_pre_existing_value_back() {
    const NAME: &str = "SHUNT_TEST_ENV_GUARD_PRE_EXISTING";

    // Seed under the lock, then release it so the guard below starts from a
    // name that already has a value — the state a developer's own environment
    // puts a test in.
    {
        let _lock = common::set_env_blocking(&[]);
        std::env::set_var(NAME, "ambient");
    }

    {
        let mut vars = common::set_env_blocking(&[(NAME, "overridden")]);
        assert_eq!(std::env::var(NAME).as_deref(), Ok("overridden"));
        vars.unset(NAME);
        assert_eq!(
            std::env::var_os(NAME),
            None,
            "the body can still remove it mid-test"
        );
    }

    {
        let _lock = common::set_env_blocking(&[]);
        assert_eq!(
            std::env::var(NAME).as_deref(),
            Ok("ambient"),
            "the value the test found must come back, not merely be removed"
        );
        std::env::remove_var(NAME);
    }
}

/// `unset` exists for a test asserting what happens when a variable is *absent*.
/// A removal writes `environ` like any other write, so it goes through the guard
/// — and it has to actually remove, or such a test asserts nothing.
#[test]
fn unset_removes_the_variable_for_the_rest_of_the_test() {
    const NAME: &str = "SHUNT_TEST_ENV_GUARD_UNSET";

    let mut vars = common::set_env_blocking(&[(NAME, "present")]);
    assert_eq!(std::env::var(NAME).as_deref(), Ok("present"));
    vars.unset(NAME);
    assert_eq!(
        std::env::var_os(NAME),
        None,
        "the rest of the body must observe the variable as missing"
    );
    drop(vars);

    // The name was absent before the test, so restoring it is a second removal.
    // That path has to be a no-op rather than a panic.
    let _after = common::set_env_blocking(&[]);
    assert_eq!(std::env::var_os(NAME), None);
}

/// The restore is a `Drop` precisely so a failing assertion cannot skip it. A
/// cleanup line at the end of a test body is exactly what this replaces, so the
/// unwinding case is the one worth pinning down: if it did not hold, one failing
/// test would leave its variables set for every test that follows.
#[test]
fn the_guard_restores_the_environment_when_the_test_panics() {
    const NAME: &str = "SHUNT_TEST_ENV_GUARD_PANIC";

    let unwound = std::panic::catch_unwind(|| {
        let _vars = common::set_env_blocking(&[(NAME, "set before the panic")]);
        assert_eq!(std::env::var(NAME).as_deref(), Ok("set before the panic"));
        // Expected — the panic message below is part of this test passing.
        panic!("deliberate: asserting the guard restores while unwinding");
    });
    assert!(
        unwound.is_err(),
        "the body above must actually have panicked"
    );

    // Taking the lock here also proves it was released while unwinding: it lives
    // in the same guard, so reaching this at all means `Drop` ran.
    let _after = common::set_env_blocking(&[]);
    assert_eq!(
        std::env::var_os(NAME),
        None,
        "a panicking test must not leak its variables to the next one"
    );
}
