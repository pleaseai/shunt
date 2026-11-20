---
name: codeql-cleartext-alerts-in-tests-reword-first
description: CodeQL cleartext alerts on pool tests are cleared by keeping the flagged value out of the assertion or log message; dismissal is only for loopback token posts and needs a maintainer
type: gotcha
source: "PR #884 (CodeQL alerts on new pool tests); existing fixes at src/model/antigravity_request.rs:669 and tests/admin_surface.rs:4766"
---

# CodeQL cleartext alerts in tests: reword first

A PR that adds pool tests can turn the `CodeQL` check red with high-severity
cleartext alerts even though nothing leaks. Fix the code first; dismiss only when
rewording is impossible.

- **`rust/cleartext-logging`** fires on an assertion or log message that formats an
  identifier such as `account.name` or a session id. The rule matches the
  identifier, not the value. **Primary fix:** keep the flagged value out of the
  message and print an index or a fixture marker instead. The repo already does
  this: `src/model/antigravity_request.rs:669` asserts on digit counts and never
  formats the session id, and `tests/admin_surface.rs:4766` names the matched
  literal marker instead of echoing the response body. Do the same for new tests.
- **`rust/cleartext-transmission`** fires on a test that posts a dummy gateway
  token (`x-shunt-token: gateway-secret`) over `http://` to a loopback gateway the
  test starts itself. Rewording does not help when the post is the point of the
  test. A maintainer dismisses these as "used in tests"; earlier dismissals are
  listed by `gh api 'repos/pleaseai/shunt/code-scanning/alerts?state=dismissed'`.

An agent session cannot dismiss alerts itself, because the auto-mode classifier
blocks `PATCH …/code-scanning/alerts/<n>` as a CI bypass. Hand the alert numbers
and a suggested reason to a maintainer.
