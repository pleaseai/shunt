## Acknowledgment

Hello. We have received your request. Here is how we understand it.

**Request**: The workload token exchange that lets GitHub Actions workflows sign in to shunt has
no limit on how often it can be called. A caller holding one valid workflow identity token can
keep trading it for new shunt tokens. A caller sending invalid tokens can keep shunt busy
checking them. You asked for both to be limited.

**Current status**: Scoped. The plan adds two limits, both on by default:

- how many tokens one workflow identity can obtain in a time window;
- how many failed attempts one client address can make in a time window.

Operators can tune both limits.

**Impact**: Once shipped, a workflow that stays within the limits sees no change. A caller over a
limit is told to wait, with a standard "retry after" hint. Repeated failures from one address are
turned away before shunt spends any effort checking them. Deployments behind a trusted proxy keep
using their existing proxy setting, so each caller is counted by its own address.

**Next steps**: We will implement the limits, document the new settings and their defaults, and
let you know when the change is ready for review.

If anything above does not match your request, please let us know. Thank you.

## Resolution

Hello. The change you requested is ready, and here is a summary.

**What was wrong**: Nothing limited how often a workflow could trade its GitHub Actions identity
token for a shunt token. A caller with one valid token could keep obtaining new shunt tokens, and a
caller sending invalid tokens could keep shunt busy checking them.

**What we changed**: shunt now applies two limits, both on by default and both adjustable by the
operator:

- how many tokens one workflow identity can obtain in a time window (60 every 10 minutes by default);
- how many failed attempts one client address can make in a time window (10 every 10 minutes by
  default).

A caller over either limit is told to wait, with a standard "retry after" hint giving the number of
seconds. Once an address is over the failed-attempt limit, shunt turns its requests away before
spending any effort checking them. Deployments behind a trusted proxy can use their existing proxy
setting, so each caller is counted by its own address.

**What to expect**: A workflow that stays within the limits sees no change. To adjust the limits,
operators set them in the shunt configuration and restart shunt.

**Release status**: The change is in pull request #917, now under review. It will ship in a release
after it is merged.

Thank you for raising this. Please let us know if anything does not work as described.
