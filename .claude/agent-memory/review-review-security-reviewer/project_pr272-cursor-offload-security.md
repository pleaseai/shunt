---
name: pr272-cursor-offload-security
description: PR #272 Cursor offload — head-of-line blocking was real and is fixed; the queued-payload DoS claim was reviewed and rejected as a pre-existing gateway-wide property (#260)
metadata:
  type: project
---

Two claims were raised against PR #272's `spawn_bounded` admission. Their dispositions differ, and
both were settled during the review loop — do not re-raise either as a new finding without new evidence.

**Fixed — cross-workload head-of-line blocking.** The first draft shared one `cpu_slots()` semaphore
across request framing, image decode, and response-path gzip, so a burst of request preparation could
delay decompression of an already-streaming response. Commit `b73f31a` split it into
`offload::gzip_slots()` and `offload::request_prep_slots()`, each CPU-sized independently, so
request-path bursts can no longer stall in-flight response frames.

**Rejected — queued waiters retain their payloads.** The claim was that FIFO waiters hold owned
prompt/image/tool data (and, for image decode, the parsed request) while parked, amplifying resident
memory into a DoS. This is a real property but not a PR #272 regression:

- A permit bounds one *in-progress* task and its working set, never total resident memory. Queued
  inputs and completed outputs live outside the permit both before and after this PR.
- Shunt has no ingress concurrency cap at all, so the same amplification is reachable today on every
  request path. That gateway-wide gap is tracked separately as issue **#260** and is out of scope here.
- Peak decoded bytes actually *improve* on this path: the pre-PR inline decode materialized every
  image eagerly, whereas the offloaded path admits at most `request_prep_slots()` decodes at once.

**Why:** severity depends on whether a change *introduces* the exposure or inherits it. Inherited,
already-tracked properties belong in their own issue, not as a merge gate on an unrelated perf PR.

**How to apply:** keep the verified-sound controls — strict base64 `STANDARD` upper-bound estimate,
the 64 MiB decompression cap plus 32 KiB inline output probe, and the permit held *inside* the
`spawn_blocking` closure so cancellation or panic releases it only when the unabortable work exits
(mutation-proven by `cancelling_the_caller_keeps_the_permit_until_the_task_exits`). If revisiting
memory amplification, argue it at the ingress layer under #260 rather than at this semaphore.
