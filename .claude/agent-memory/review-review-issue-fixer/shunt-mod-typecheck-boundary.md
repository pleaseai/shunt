---
name: shunt-mod-typecheck-boundary
description: plugins/shunt typechecks every module except hooks/register.ts, which needs an engine-generated claude-code.d.ts that only exists after /plugin-types
metadata:
  type: project
---

`plugins/shunt` has a `typecheck` script (`tsc --noEmit`) gated in CI, but its
`tsconfig.json` **excludes `hooks/register.ts`**.

**Why:** `register.ts` is the only module importing `claude-code`. Those types
come from an engine-generated `claude-code.d.ts` that Claude Code writes into a
plugin author's project via `/plugin-types`; it is not in the repo and not in
CI, so including the file would make the gate fail unconditionally.

**How to apply:** when changing `register.ts`, `tsc` will not catch type errors
for you. Verify it locally against a real `claude-code.d.ts` with a throwaway
tsconfig that maps `paths: { "claude-code": [...] }` at a copy of the engine
d.ts — that is the only way to prove an engine API (e.g. `Registration.catch`,
`CatchHandler`) is used correctly. Never "fix" a register.ts type error by
loosening the shared tsconfig; the exclusion is deliberate and documented in
`plugins/shunt/README.md`, the tsconfig comment, and the CI step comment.

Related: [[shunt-mod-blank-env-override]].
