---
name: shunt-mod-blank-env-override
description: In this repo, a blank override must be normalized to absent BEFORE the fallback is selected — `??` is the recurring bug shape, `?.trim() ||` is the fix shape
metadata:
  type: feedback
---

When one value overrides another and either can come from an environment
variable, a header, or any external string source, normalize blank to absent
**before** the selection, not after it. The bug shape is `a ?? b`; the fix shape
is `a?.trim() || b`.

**Why:** `??` falls back only on `null`/`undefined`, but an exported-but-empty
variable reads as `''` (`$.env.get`'s documented contract) — so a blank override
shadows a perfectly valid fallback and the code reports "nothing configured"
while the user has it configured. This repo has hit the identical defect class
repeatedly: commit `5154078b` ("treat a blank session header as no session",
#568) and then again one commit later in
`plugins/shunt/hooks/endpoint.ts` (`SHUNT_BASE_URL`, `SHUNT_TOKEN`).

**How to apply:** on any `??` or `||` chain over external strings, ask whether
`''` is reachable. A downstream `x.trim() === ''` check does **not** fix it —
by then the fallback was already discarded. Regression tests must pair the
blank override *with a valid fallback*; a test where the blank value is the only
value passes against the unfixed code and proves nothing.

Related: [[shunt-mod-typecheck-boundary]].
