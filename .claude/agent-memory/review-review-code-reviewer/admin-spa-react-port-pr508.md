---
name: admin-spa-react-port-pr508
description: PR #508 ported src/admin/script.rs (646-line inline JS) to ui/ React/TS; one extraction logic error was missed by this review, caught later and fixed before merge; verification method and its blind spot for a pure-port PR.
metadata:
  type: project
---

PR #508 (branch `amondnet/admin-spa-port`) is the first of two PRs in ADR-0003
item 4: porting the server-rendered dashboard's JS (`src/admin/script.rs`,
`dashboard_page` in `src/admin/html.rs`) into `ui/` React/TS, without deleting
the original — `GET /admin` still serves server-rendered HTML. Nothing here
generalizes to future PRs' code, only the verification method, since the port
turned out clean.

**Verification method that worked:** since the ground-truth JS reference
(`script.rs`) stays in the tree during a port, diff every ported function
against its original line-for-line, including *preserving quirky
inconsistencies* rather than "fixing" them — e.g. `accountGroups`'s embedded
`state` ternary and `loadPool`'s local `state` ternary use a different order
from each other in BOTH the original and the port; that is not a bug, and a
port that "fixed" it to be consistent would itself be the defect (silently
changing displayed status for pool-only rows).

**Outcome:** the port was unusually faithful across every flagged risk area
(`accountGroups`/`effectiveState`/`rowStatusText`, provisioning epoch/completing
guards, all five `loadX` functions, presentational components) — zero
logic-error findings. The only new server-side surface, `GET /admin/api/session`
(returns `{csrf, expiry_buffer_ms}` since the SPA shell is a static file that
can't have per-session values interpolated at compile time), has solid test
coverage in `tests/admin_surface.rs` covering unauthenticated/header/cookie
credential paths and CSRF. Docs (`docs/admin-ui-delivery.md` path-count table,
all 4 site locales) were updated in step per AGENTS.md.

**Correction — the method has a blind spot, and it cost a finding here.**
"Zero logic-error findings" was wrong. Diffing ported *functions* against the
original misses divergence introduced by the *extraction* itself: the port
factored every mutation through one new `mutate()` helper in `ui/src/api.ts`,
and no single function in `script.rs` corresponds to it. `mutate()` wrapped the
body parse in its own `try/catch`, which flattened a distinction the original
draws deliberately across three call sites — `completeClaude`'s
`await res.json()` is bare (an unreadable body escalates to "the account may
still have been stored", tables re-read), while `removeAccount`/`refreshAccount`
use `.catch(() => ({}))` (tolerant). The port gave every caller the tolerant
form, so a completion whose answer the page could not read reported a definite
"Failed to complete" and skipped the table refresh — on a single-use
authorization code, i.e. it told an operator to retry an exchange that may
already have stood. Fixed in this PR via an `answered` discriminator consumed
only by the completion path.

So: when a port introduces a shared helper, diff the helper against **every**
call site it replaced, and treat a spot where the original was inconsistent as
load-bearing until proven otherwise. The rule above ("preserve quirky
inconsistencies") was right — this is the same rule applied to error paths,
which is where it was not applied.

See [[shunt-codex-websocket-v2]] pattern of thorough memory for future admin
SPA port PRs (the follow-up PR deleting the server-rendered path).
