---
name: shunt-catalog-cache-sibling-map-eviction
description: shunt antigravity catalog module keeps two maps (CATALOG_CACHE, CATALOG_FETCH_SLOTS) keyed identically — an eviction fix applied to one silently misses the other (caught in PR #451 review, fixed before merge)
metadata:
  type: project
---

In `src/auth/antigravity/catalog.rs`, `CATALOG_CACHE` and `CATALOG_FETCH_SLOTS` are two
separate `HashMap`s keyed by the same string (`cache_key(base_url, access_token, project_id)`
— backend plus Code Assist project id, with a token fingerprint only for a project-less
credential, as merged in PR #451). While that PR was in review, an intermediate working-tree
state added `evict_expired()` to fix `CATALOG_CACHE`'s unbounded growth from per-credential
keys but not the equivalent for `CATALOG_FETCH_SLOTS` — same growth pattern, no cleanup at
all. Both cubic and manual review independently converged on this as the single real finding
in that diff (P2/minor — slow, small leak). It was fixed before the commit landed: the merged
`evict_expired()` sweeps both maps (a slot goes when its cache entry is gone and the map holds
the last `Arc` reference).

**Why:** the two maps look independent (different locks: `std::sync::Mutex` for the cache,
also `std::sync::Mutex` for slots but holding a `tokio::sync::Mutex` inside) so a fix to one
doesn't visually demand a matching fix to the other, but they share a key space by
construction — any change to what the key encodes (e.g. adding the token fingerprint) affects
both maps' growth rate identically.

**How to apply:** when reviewing a change to `cache_key` / the keying scheme in this file,
check `CATALOG_FETCH_SLOTS` too, not just `CATALOG_CACHE` — grep for every
`static ... LazyLock<Mutex<HashMap<String, ...>>>` in the file and confirm each has an
eviction path if its key can grow unboundedly. This is one instance of a general pattern: a
sibling data structure silently missing a fix applied to its counterpart, so enumerate the
siblings before calling the fix complete.
