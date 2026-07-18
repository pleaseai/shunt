---
name: shunt-client-usage-fable-coverage
description: PR #179 client usage aggregate tests omit the fable 7d_oi window despite wiring it in production
metadata:
  type: reference
---

PR #179's usage aggregate tests cover 5h and 7d but the fixture leaves utilization_7d_oi/reset_7d_oi unset, so the fable mapping at usage.rs aggregate cannot regress visibly. When reviewing future usage/quota surfaces, require one fixture with populated fable data and reset assertion.
