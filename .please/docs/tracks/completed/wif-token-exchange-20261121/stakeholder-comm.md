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

## Resolution

Hello. The request you made is now implemented and waiting for review.

**What was missing**: shunt could not accept the token exchange that gh-aw uses for keyless sign-in. A gh-aw run therefore needed a stored secret, or failed once its short-lived GitHub Actions token expired mid-run.

**What we changed**: shunt now accepts the same token exchange gh-aw already sends for Anthropic Workload Identity Federation. An operator turns it on and opts a workload identity rule into it. gh-aw then trades its GitHub Actions token for a shunt token and refreshes that token on its own, so a run of any length works with no stored secret. Removing the rule, or changing which identities it accepts, cancels the tokens it issued, as does retiring the signing secret. Settings that only control how the sign-in is checked at exchange time (where the provider's keys come from, allowed clock drift, and maximum sign-in age) do not cancel them.

**How to check it**: Follow the GitHub Actions OIDC guide in the shunt documentation. It has a ready-to-copy shunt configuration and gh-aw workflow settings. Run the workflow and confirm it reaches the model through shunt with no static token configured. Usage is attributed to the repository identity, the same as direct OIDC use.

**Release status**: The change is in PR #915, which is under review. It reaches a release after it merges. Before merge we will run it once against a real GitHub Actions workflow. Rate limiting for the new endpoint follows separately (#914).

Thank you for the request. If anything does not work as described, please let us know.
