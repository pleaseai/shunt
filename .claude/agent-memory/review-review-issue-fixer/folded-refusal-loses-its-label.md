---
name: folded-refusal-loses-its-label
description: libsy folds a refused CallModel into its own fall-open, so shunt's outcome label needs a request-local note per refusal reason, not just the failure slot
metadata:
  type: project
---

In `src/routing/driven/drive.rs`, every terminal branch reads its
`judge_outcome` from one closure over a request-local record. The record used to
hold only `Mutex<Option<JudgeFailure>>` — "a call was made and failed" — so a
`max_judge_calls` refusal inside the drive closure (which never reaches
`judge_call`) left it `None` and the turn was labelled `invalid_reply`.

**Why:** libsy folds *every* refused or failed `CallModel` into the same "no
verdict" fall-open, so the algorithm's own outcome cannot tell the reasons
apart; only shunt's side of the closure knows which happened.

**How to apply:** when adding a new way for the drive closure to refuse or short
-circuit a call, add a note to `DriveNotes` for it too — otherwise the metric
`shunt.router.judge_calls{outcome}` blames the judge. The fast-path check at the
top of `drive` is not enough: it only covers a budget already spent before the
drive, not a chaining algorithm's second call.
