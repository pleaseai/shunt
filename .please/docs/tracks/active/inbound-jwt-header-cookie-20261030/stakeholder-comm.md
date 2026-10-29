## Acknowledgment

Hello. We've received your request and logged it. Here is our understanding of it.

**Request**: Let developers sign in to a shared shunt gateway from the Codex CLI with their organization's single sign-on (for example Microsoft Entra ID), using Codex's built-in "gateway OAuth" sign-in. Today Codex sends that sign-in token in a place shunt doesn't look, so every request is refused.

**Current status**: Confirmed. Requirements are written and implementation planning is next.

**Impact**: Teams that want per-person sign-in, usage attribution, and spend limits for Codex can't use Codex's built-in sign-in yet. Until this ships, a workaround is available: a small helper command that fetches the sign-in token and hands it to Codex in the standard place.

**Next steps**: We'll add a setting that tells shunt where to find the sign-in token (a named header or cookie), and document how to set it up with Microsoft Entra ID, including the identity-provider settings that commonly trip people up. We'll follow up when it's ready to try.

If we've misunderstood anything, or you need a particular identity provider covered, please let us know. Thank you.
