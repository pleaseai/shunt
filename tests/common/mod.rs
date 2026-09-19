//! Helpers shared by the integration test binaries.
//!
//! `mod common;` pulls this in. Each binary compiles its own copy of everything
//! here, which is exactly right for [`EnvVars`]: the hazard it guards is
//! per-process, and the test binaries are separate processes.

// Every binary uses a different part of this module. The rest is not dead code,
// it is code this binary happens not to need.
#![allow(dead_code)]

/// Serializes every test in a binary that touches the process environment.
///
/// The hazard is not two tests sharing a variable name — it is `setenv` itself.
/// It may reallocate the single global `environ` array, so a write to *any*
/// variable can be observed by a concurrent `getenv` for an *unrelated* one as a
/// missing value. The gateway calls `env::var(token_env)` per request while
/// resolving a `token_env` account (`src/auth/mod.rs`), and `AdminConfig::resolve`
/// reads `tokens_env` as the router is built, so an unsynchronized `set_var` in a
/// sibling test makes that account look unconfigured, the pool run out of
/// candidates, and the request fail with 502 (issue #507).
///
/// So the rule is not "hold this when you touch the refresh env vars"; it is
/// **hold it for the whole body of any test that touches the environment at
/// all** — writers and readers alike, and across the read that resolves the
/// config as well as the cleanup. Holding it for the write alone excludes
/// nothing.
///
/// It also has to be *one* lock. Per-variable-family locks look like
/// serialization and provide none: a writer holding the `SHUNT_CLAUDE_*` lock
/// still reallocates `environ` under a reader holding the `SHUNT_ADMIN_*` one.
/// `tests/admin_surface.rs` had three such locks, so 32 of its tests "held the
/// lock" and none of them were serialized against the other two groups
/// (issue #539).
///
/// A tokio mutex, because it is held across the `.await`s of a request.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Process environment variables owned by one test, restored when it ends.
///
/// Restored, not merely removed: the value each name had before the test is put
/// back, so a variable the surrounding environment had set survives a test that
/// overrode it. For the usual case — a `SHUNT_TEST_*` name nothing else sets —
/// that is a removal.
///
/// The restore is a `Drop`, so it happens when an assertion, an `unwrap`, or
/// the request itself panics — which a cleanup line at the end of a test body
/// cannot do. [`ENV_LOCK`] is released only after it, so no sibling observes the
/// variables on their way out.
///
/// Bind it for the whole test — `let _env = common::set_env(..).await;`. Do not
/// write `let _ = ...`: that drops the guard immediately, releasing the lock and
/// unsetting the variables before the test has done anything.
///
/// The `#[must_use]` below catches only the bare-statement form of that mistake
/// (`common::set_env(..).await;` with no binding at all). It cannot catch
/// `let _ = ...`, which is the idiom rustc itself suggests for *silencing* the
/// lint — so that spelling stays a rule this comment enforces and the compiler
/// does not. `tests/env_lock_coverage.rs` cannot see it either: the call goes
/// through this guard, which is exactly what that gate scans for.
#[must_use = "binding this to `_` drops the guard immediately, releasing the \
              lock and restoring the environment before the test body runs"]
pub struct EnvVars {
    /// Each name this guard has touched, with the value it held beforehand.
    /// First touch wins: a test that sets the same name twice still restores
    /// what was there before *it* ran, not what its own first write left.
    saved: Vec<(String, Option<std::ffi::OsString>)>,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl EnvVars {
    /// Sets one more variable under the lock this guard already holds, for a
    /// value that exists only later in the test — a mock server's URL, say.
    pub fn set(&mut self, name: &str, value: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.remember(name);
        std::env::set_var(name, value.as_ref());
        self
    }

    /// Removes a variable for the rest of the test, for one asserting what
    /// happens when it is *absent*. A removal writes `environ` like any other
    /// write, so it belongs under the lock too — and the name stays on the
    /// cleanup list, which makes a second removal at drop a harmless no-op.
    pub fn unset(&mut self, name: &str) -> &mut Self {
        self.remember(name);
        std::env::remove_var(name);
        self
    }

    fn remember(&mut self, name: &str) {
        if self.saved.iter().any(|(seen, _)| seen == name) {
            return;
        }
        self.saved.push((name.to_string(), std::env::var_os(name)));
    }
}

impl Drop for EnvVars {
    fn drop(&mut self) {
        for (name, previous) in &self.saved {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// Takes [`ENV_LOCK`] and sets `vars` for the rest of the test body.
pub async fn set_env(vars: &[(&str, &str)]) -> EnvVars {
    let mut guard = EnvVars {
        saved: Vec::with_capacity(vars.len()),
        _lock: ENV_LOCK.lock().await,
    };
    for (name, value) in vars {
        guard.set(name, value);
    }
    guard
}

/// Takes [`ENV_LOCK`] without setting anything yet, for a test whose values are
/// all computed later. Add them with [`EnvVars::set`].
pub async fn env_lock() -> EnvVars {
    set_env(&[]).await
}

/// [`set_env`] for a `#[test]` with no runtime to await on.
///
/// Panics when called from an async context — that is `blocking_lock`'s own
/// contract, and a `#[tokio::test]` reaching this function is a mistake the
/// panic names immediately.
pub fn set_env_blocking(vars: &[(&str, &str)]) -> EnvVars {
    let mut guard = EnvVars {
        saved: Vec::with_capacity(vars.len()),
        _lock: ENV_LOCK.blocking_lock(),
    };
    for (name, value) in vars {
        guard.set(name, value);
    }
    guard
}

/// Matches on the forwarded body's `model` field alone.
///
/// The classifier and pool paths also rewrite `system` — and the pool path the
/// account uuid — so pinning the whole body would couple a test to mutations it
/// is not about.
pub struct BodyModelIs(pub &'static str);

impl wiremock::Match for BodyModelIs {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
            return false;
        };
        body.get("model").and_then(serde_json::Value::as_str) == Some(self.0)
    }
}
