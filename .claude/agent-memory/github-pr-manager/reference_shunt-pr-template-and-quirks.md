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

`gh please pr create --repo "$OWNER_REPO" --draft ...` printed **no stdout at
all** on a successful create (both title and body args accepted, empty
output). Don't treat empty output as failure — verify with
`gh pr list --head "$BRANCH" --repo "$OWNER_REPO"` (plain `gh`, not `gh
please pr list`, which errored with exit 1 / a stray `--json` flag artifact
in this environment) before assuming the create failed and retrying.

**Worse variant confirmed on PR #39**: `gh please pr create ... --body-file -`
fed via a bash heredoc (`<<'PRBODY' ... PRBODY`) silently created the PR with
an **empty body** — no error, exit 0, title set correctly, but
`gh pr view "$PR_NUMBER" --json body` came back `""`. Always verify body content
after create (`gh pr view "$PR_NUMBER" --json body --jq .body | head`), not just
that the PR exists. Fix: write the body to a real file with the Write tool and
run plain `gh pr edit "$PR_NUMBER" --repo "$OWNER_REPO" --body-file "$BODY_FILE"`
— that reliably set a 3000+ char body. Prefer writing the body to a file and
using `--body-file "$BODY_FILE"` (not `-`/stdin/heredoc) on the initial
`gh please pr create` call too, to avoid the empty-body failure mode altogether.

pleaseai/shunt is not a Graphite repo (`detect-stack-tool.sh` prints nothing) —
plain `gh please pr create` / `gh pr create` is the right tool, no `gt submit`.

**Confirmed fix works on initial create too (PR #44)**: passing
`--body-file "$BODY_FILE"` (Write-tool-authored file, not stdin/heredoc) directly
on the first `gh please pr create --draft ...` call produced a correct non-empty
body (verified via
`gh pr view "$PR_NUMBER" --json body --jq '.body | length'`) — no need to
create-then-edit. Command still printed no stdout on success;
`gh pr list --head "$BRANCH" --repo "$OWNER_REPO" --json number,title,isDraft,url`
(plain gh) is the reliable way to confirm the PR exists and get its number.
