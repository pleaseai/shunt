## Acknowledgment

Hello. We've received your request and logged it. Here is our understanding of it.

**Request**: Let developers sign in to a shared shunt gateway from the Codex CLI with their organization's single sign-on (for example Microsoft Entra ID), using Codex's built-in "gateway OAuth" sign-in. Today Codex sends that sign-in token in a place shunt doesn't look, so every request is refused.

**Current status**: Confirmed. Requirements are written and implementation planning is next.

**Impact**: Teams that want per-person sign-in, usage attribution, and spend limits for Codex can't use Codex's built-in sign-in yet. Until this ships, a workaround is available: a small helper command that fetches the sign-in token and hands it to Codex in the standard place.

**Next steps**: We'll add a setting that tells shunt where to find the sign-in token (a named header or cookie), and document how to set it up with Microsoft Entra ID, including the identity-provider settings that commonly trip people up. We'll follow up when it's ready to try.

If we've misunderstood anything, or you need a particular identity provider covered, please let us know. Thank you.

## Resolution

Hello. The change you asked for is built and ready for review, and here is where it stands.

**What was wrong**: Codex CLI's built-in "gateway OAuth" sign-in sends each developer's single sign-on token in a named cookie or a custom header. shunt only looked for that token in the standard authorization header, so every Codex request signed in this way was refused.

**What we changed**: Two new settings tell shunt where Codex puts the sign-in token: a header name or a cookie name. shunt checks the token with your existing identity-provider rules, records usage and spending limits under each developer's own identity, and never passes the token on to the model provider. The setup guide now walks through Microsoft Entra ID, including the identity-provider settings that commonly trip people up.

**How to check it**: Set the header or cookie name that matches your Codex `gateway_oauth` configuration, sign in from the Codex CLI, and complete a turn. The usage for that turn should appear under your own identity rather than a shared token.

**Release status**: The change is in review as PR #862 and has passed our automated tests, but it has not yet been tried end to end with a real Codex CLI sign-in. It ships once that PR is merged.

Thank you for the detailed report. If you hit anything unexpected, especially with a different identity provider, please let us know.
