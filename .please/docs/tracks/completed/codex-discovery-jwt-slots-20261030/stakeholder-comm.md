## Resolution Report

Hello. The issue you reported has been resolved, and here is a summary.

**What was wrong**: When a Codex CLI user was signed in to the organization's gateway through the company identity provider and also kept ChatGPT sign-in on, Codex could not load its model list from the gateway. Every model-list request was refused, so Codex logged an error and used its built-in list instead.

**How we fixed it**: The gateway's model-list endpoints now accept the same sign-in header or cookie that the gateway's Codex request endpoints already accept, and they check it with the same rules. Existing sign-in methods (static tokens, the gateway login token) work as before, and deployments that do not use the sign-in header or cookie see no change.

**How to check it / expected behavior**: With both sign-ins on, Codex's model-list request now succeeds instead of logging an authentication error. The list the gateway returns is still empty for now, so Codex keeps using its built-in models; serving the gateway's real model list is planned as a separate follow-up.

**Release status**: The fix is in PR #883, which is under review. It will ship in the next release after it merges.

Sorry for the inconvenience. If you run into anything else, please let us know. Thank you.
