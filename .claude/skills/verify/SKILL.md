---
name: verify
summary: Runtime verification for shunt provider changes
---

# Verify shunt provider changes

Use `.claude/skills/run-shunt/SKILL.md` for the standard gateway launch pattern.
For protocol adapters, run a local mock upstream that emits the provider's wire
format, point a temporary provider config at it, and drive `/v1/messages` with
`curl` in both JSON and `stream: true` modes. Capture the mock's received path,
headers, and body prefix, and probe malformed model/config errors. Use isolated
ports and temporary credentials under the session scratchpad; terminate both
processes after capture.
