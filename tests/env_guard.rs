//! The behaviour of the shared environment guard itself.
//!
//! Every other test binary depends on `common::EnvVars` doing three things, and
//! nothing in those binaries asserts any of them — they use the guard for their
//! own purposes and would still pass if its restore were subtly wrong, which is
//! how a guard silently stops guarding. These are the three.

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
    assert_eq!(
        std::env::var_os(NAME),
        None,
        "both writes restore to the value that was there before the test"
    );
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

    // Reaching this at all also proves the lock was released: it lives in the
    // same guard, so the restore having run means `Drop` ran.
    assert_eq!(
        std::env::var_os(NAME),
        None,
        "a panicking test must not leak its variables to the next one"
    );
}
