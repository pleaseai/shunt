---
name: headermap-get-vs-append
description: In shunt, outbound HeaderMaps are built with append, so get()/insert() header rewrites silently drop or miss repeated field lines
metadata:
  type: project
---

`crate::headers::filtered` (`src/headers.rs:15`) builds the outbound map with
`HeaderMap::append`, so any header a client sent as multiple field lines arrives
as a multi-valued entry. Rewrite helpers that read with `get()` but write with
`insert()`/`remove()` are therefore asymmetric: the read sees only field 1, the
write replaces all fields.

**Why:** fixed in `src/adapters/anthropic/safeguards.rs::strip_safeguard_betas`
— it could drop legitimate `anthropic-beta` tokens or let a
`dangerous-tool-use-*` token through to a third-party host (the exact 400 the
module exists to prevent).

**How to apply:** for any comma-list header rewrite, aggregate with
`headers.get_all(name).iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(",")`
first; the single `remove`-or-`insert` write-back is then correct. Regression
tests must build the map with `append`, not `insert`, or they are vacuous.
