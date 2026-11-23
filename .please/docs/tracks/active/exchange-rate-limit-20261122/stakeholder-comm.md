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
