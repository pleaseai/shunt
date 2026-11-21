## Acknowledgment

Hello. We have received your request. Here is how we understand it.

**Request**: GitHub Agentic Workflows (gh-aw) cannot sign in to shunt with GitHub Actions OIDC
today. The short-lived OIDC token expires mid-run, and the sandboxed agent cannot request a
fresh one. You asked shunt to accept the same token exchange that gh-aw already uses for
Anthropic Workload Identity Federation. A workflow could then point gh-aw at shunt and run
without a stored secret.

**Current status**: Scoped and planned. We have confirmed the exact request format that gh-aw's
proxy sends, so shunt can accept it as is.

**Impact**: Once shipped, an operator can opt a workload identity rule into the exchange. A gh-aw
workflow then runs through shunt for any length of time with no static token. Usage and spend
limits are attributed to the same repository identity as direct OIDC use. Deployments that do not
turn the feature on see no change.

**Next steps**: We will implement the exchange, document a ready-to-copy gh-aw configuration, and
let you know when it is ready for review. Rate limiting of the new endpoint will follow as a
separate item.

If anything above does not match what you need, please let us know. Thank you.
