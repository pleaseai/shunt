# Memory index

- [cubic empty review on clean tree](cubic-empty-review-clean-tree.md) — `-j` without `-b` on a clean tree = silent false-clean `{"issues":[]}`; always add `-b <base>` for committed-branch reviews.
- [Sentry error Display URL leak (#310)](shunt-sentry-error-display-url-leak.md) — `error_chain` in stream_metrics.rs forwards reqwest URL text past `sanitize_tag`, contradicting docs/running.md's "host name never sent" guarantee.
- [catalog cache sibling map eviction](shunt-catalog-cache-sibling-map-eviction.md) — antigravity catalog.rs `CATALOG_CACHE`/`CATALOG_FETCH_SLOTS` share a key space; an eviction fix to one map misses the other.
