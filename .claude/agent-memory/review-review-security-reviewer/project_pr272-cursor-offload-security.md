---
name: pr272-cursor-offload-security
description: PR #272 shared Cursor CPU queue retains owned request data and creates cross-workload head-of-line blocking; other controls verified sound
metadata:
  type: project
---

PR #272's FIFO `spawn_bounded` queue can retain each waiting Cursor request's owned prompt/images/tools; image decode waiters additionally retain the parsed/raw request because tool extraction follows the await. With a 64 MiB inbound cap and no ingress concurrency cap, concurrent large authenticated requests can amplify resident memory and cause process-level DoS. Sharing the same queue across request framing, image decode, and gzip also creates cross-workload head-of-line blocking.

**Why:** The semaphore limits only executing closures, not queued closures or their captured payloads; unlike the pre-PR inline image decode/framing, the new path yields while retaining those inputs.

**How to apply:** Prefer bounded admission with queue shedding (or a strict bounded queue/ingress concurrency limit) before capturing large payloads. Keep the verified-safe controls: strict base64 `STANDARD` estimate, 64 MiB decompression cap + 32 KiB inline output probe, and moving the permit inside `spawn_blocking` so panic/cancellation releases it only when work exits.
