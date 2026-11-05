---
name: codeql-flags-test-account-names-and-loopback-tokens
description: CodeQL raises high-severity cleartext alerts on test-only account-name assert messages and loopback token posts; they are false positives that a maintainer must dismiss
type: gotcha
source: "PR #884 (CodeQL alerts #57, #58, #59); earlier dismissals on tests/multi_account.rs and tests/admin_surface.rs"
---

# CodeQL flags test account names and loopback tokens

A PR that adds pool tests can turn the `CodeQL` check red with "3 high severity"
alerts even though nothing leaks:

- **`rust/cleartext-logging`** fires on an `assert_eq!(…, "{}", account.name)`
  failure message. The rule matches the identifier `account`, not the value; the
  fixtures carry a label (`"first"`, `"account-a"`) and no token material. PR #884:
  alerts #58/#59 at `src/accounts.rs:11681,11683`.
- **`rust/cleartext-transmission`** fires on a test that posts a dummy gateway
  token (`x-shunt-token: gateway-secret`) over `http://` to a loopback gateway the
  test starts itself. PR #884: alert #57 at
  `tests/inbound/inbound_codex_websocket.rs:1112`.

A maintainer dismissed earlier alerts of the same shape, each with a comment
explaining why: "false positive" for the assertion messages and "used in tests"
for the loopback posts. To see them:
`gh api 'repos/pleaseai/shunt/code-scanning/alerts?state=dismissed'`.

An agent session cannot dismiss them itself, because the auto-mode classifier
blocks `PATCH …/code-scanning/alerts/<n>` as a CI bypass. Hand the alert numbers
and a suggested reason to a maintainer.
