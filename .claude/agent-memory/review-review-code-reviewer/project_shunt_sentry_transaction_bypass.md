---
name: shunt-sentry-transaction-bypass
description: sentry-rust 0.48.4 has no before_send_transaction — perf transactions bypass scrub_event; how the hostname leak was fixed and verified.
metadata:
  type: project
---

In shunt's `src/main.rs` `init_sentry`, `sentry` crate 0.48.4 sends performance transactions straight to `send_envelope` (`sentry-core/src/performance.rs::Transaction::finish_with_timestamp`), never through `prepare_event`/`before_send`. So `scrub_event` (registered only as `before_send`) never touches transactions — only error/message events go through it.

`sentry-contexts::ContextIntegration::setup` (integration.rs:75) auto-fills `ClientOptions.server_name` with the machine hostname only `if options.server_name.is_none()`. The fix: pin `server_name: Some("".into())` in `ClientOptions` at `init_sentry`, which preempts the hostname auto-fill for both event kinds at the source (rather than trying to scrub it downstream, which is impossible for transactions in this SDK version).

**Verified against the actual vendored crate source** (`~/.cargo/registry/src/index.crates.io-*/sentry-{core,contexts}-0.48.4/`) during iteration-2 review 2026-07-13: confirmed the `is_none()` guard and confirmed `Transaction::finish_with_timestamp` unconditionally does `transaction.server_name.clone_from(&opts.server_name)` with no `before_send_transaction` hook to intercept it. Error events still get `event.server_name = None` from `scrub_event`; transactions get `Some("")` (empty, not the hostname) — different final values but both non-leaking.

**How to apply:** When reviewing Sentry-related shunt changes, always check the vendored crate source under `~/.cargo/registry/src/index.crates.io-*/sentry-*-0.48.4/` rather than assuming API behavior from other SDK versions/languages — the "before_send doesn't cover transactions" gap is specific to this Rust SDK version and easy to miss. See [[shunt_responses_adapter]] for the sibling pattern of verifying vendor/upstream claims via source inspection rather than trusting comments at face value.
