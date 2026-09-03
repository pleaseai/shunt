---
name: catalog-fetch-slots-sibling-map-eviction
description: shunt Antigravity catalog cache — CATALOG_FETCH_SLOTS lacked eviction after cache keys were widened to per-credential keys (caught in PR #451 review, fixed before merge)
metadata:
  type: project
---

`src/auth/antigravity/catalog.rs` has two process-wide static maps keyed by the
same `cache_key`: `CATALOG_CACHE` (catalog entries) and `CATALOG_FETCH_SLOTS`
(single-flight locks). During the PR #451 review-feedback round, an
intermediate working-tree state widened the key from `base_url` alone (low,
stable cardinality) to a per-credential key and added `evict_expired()` —
pruning entries older than `2×CATALOG_TTL` — but wired it only into the two
`cache().insert(...)` call sites for `CATALOG_CACHE`. `CATALOG_FETCH_SLOTS`
still only grew (`slots.entry(key).or_default()`), never shrank: one
`Arc<tokio::sync::Mutex<()>>` entry per key rotation for the life of the
process.

The finding was fixed before the commit landed: the merged `evict_expired()`
sweeps both maps, and the key is now backend + Code Assist `project_id`
(token fingerprint only for a project-less credential), so a routine token
refresh no longer rotates the key at all.

**How to apply:** when reviewing a cache-keying change here (or any paired
cache + lock-slot map keyed by the same identity), check that eviction covers
every sibling map sharing the widened key space, not just the primary one.
