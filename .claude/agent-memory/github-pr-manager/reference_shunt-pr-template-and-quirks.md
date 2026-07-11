---
name: shunt-pr-template-and-quirks
description: pleaseai/shunt PR template location and a gh-please pr create display quirk (silent stdout on success)
metadata:
  type: reference
---

`pleaseai/shunt` has a single repo PR template at `.github/PULL_REQUEST_TEMPLATE.md`
(Summary / Milestone-spec / Checklist / Notes for reviewers — no generic
"Changes"/"Test Plan" sections, but a "Changes" subsection fits naturally
under Summary). Checklist enforces `cargo build`/`test`/`clippy -D warnings`/
`fmt --check`, 500-line source file cap, English-only, frozen `docs/` spec
sync, and SHA-pinned GitHub Actions — run these locally before filling the
checklist rather than checking boxes blind.

`gh please pr create --repo <owner>/<repo> --draft ...` printed **no stdout at
all** on a successful create (both title and body args accepted, empty
output). Don't treat empty output as failure — verify with
`gh pr list --head <branch> --repo <owner>/<repo>` (plain `gh`, not `gh
please pr list`, which errored with exit 1 / a stray `--json` flag artifact
in this environment) before assuming the create failed and retrying.

pleaseai/shunt is not a Graphite repo (`detect-stack-tool.sh` prints nothing) —
plain `gh please pr create` / `gh pr create` is the right tool, no `gt submit`.
