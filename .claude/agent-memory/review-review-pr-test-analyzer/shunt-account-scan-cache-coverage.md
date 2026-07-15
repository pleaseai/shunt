---
name: shunt-account-scan-cache-coverage
description: PR #163 mtime-keyed account-scan cache coverage; hit/invalidation triggers covered, but changed scan output is never asserted.
metadata:
  type: project
---

PR #163's deterministic `scan_cached` test covers first miss, same-mtime hit, changed-mtime re-scan, and no-mtime bypass; its real-filesystem resolver test covers unchanged-request reuse. Unique pid+nanos paths isolate the process-wide cache, and the sole static counting callback is used by only one awaited test, so there is no concrete parallel-test race.

**Why:** Both scans return the same `one_account()` value, so call-count assertions prove re-scanning but cannot catch a regression that invokes the scanner after invalidation while continuing to return/cache the old account list. That would break the hard no-restart account-discovery requirement.

**How to apply:** For cache invalidation tests, make the scan result change across mtimes and assert both the immediate refreshed result and the subsequent hit. Do not require a timing-sensitive real-filesystem mtime test when the injected-mtime seam plus resolver hit test already covers composition.
