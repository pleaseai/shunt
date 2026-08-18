---
name: pr391-slot-enumeration
description: PR #391 (issue #363) src/auth/slots.rs — the single credential-slot enumeration; audit verdict, and the admin session-cookie accept slot that commit 5f00473 closed (do not re-open).
metadata:
  type: project
---

PR #391 adds `src/auth/slots.rs` (`SHARED_SLOTS`, `RESERVED_SLOTS`,
`ShuntCredentials::{from_state,strip_reserved_slots,strip_consumed_slots}`) and routes
all three forward sites through it: `proxy::failover::{check_inbound_auth,headers_for_route}`,
`discovery::upstream::upstream_headers` (Passthrough branch), and
`adapters::responses::inbound::passthrough_request_headers`.

**Why:** the strip predicate had drifted from the accept predicate four times (#352 #357 #361 #356).
Single enumeration + `from_state` as the only wiring point.

**Verified safe in this audit** (do not re-litigate without a code change):
- Mirror holds for every header shape. Accept sites (`InboundAuth::{authenticate,authenticate_bearer,authenticate_client}`,
  `GatewayAuth::authenticate_bearer/authenticate_token`, `AdminAuth::authenticate_credential`)
  all read `HeaderMap::get` = **first value only**, and `strip_consumed_slots` also judges the
  first value then `remove`s **all** values — so duplicate `x-api-key`/`authorization` cannot
  smuggle a gate-accepted credential past the strip. Strip is over-inclusive in the safe direction
  (checks Bearer payload against admin keys even though admin never accepts Bearer).
- The `admin_header`-names-a-shared-slot case is covered on every path that skips
  `strip_consumed_slots`: failover off-origin early return and the credential-injecting branch
  both `remove("authorization")`+`remove("x-api-key")`; codex inbound lists both in
  `PASSTHROUGH_STRIP_REQUEST_HEADERS`; discovery builds its outbound map from scratch.
- Removing `x-shunt-token`/`x-shunt-inbound-client` from `PASSTHROUGH_STRIP_REQUEST_HEADERS` is
  coverage-equivalent (both are in `RESERVED_SLOTS`, removed unconditionally).
- Discovery's scratch-`HeaderMap` restructure preserves which value is forwarded;
  `strip_duplicate_oauth_api_key` runs after the loop so insert order does not matter.
- `AppState` has exactly three credential kinds (`inbound_auth`, `admin_auth`, `gateway_auth`);
  `from_state` covers all three. Compiles; `cargo test --lib` 1592 pass.

**Closed in this PR — do not re-open.** An earlier revision of this note recorded the
`admin::authenticate` session-cookie path as a residual gap. It was, until commit `5f00473`:
`shunt_admin_session` in the `cookie` header is a full-write admin credential, and no forward
site stripped `cookie`. It is now enumerated in the `slots.rs` module doc, `"cookie"` is in
`RESERVED_SLOTS`, and `strip_reserved_slots` removes it by name at every forward site
(`proxy/failover.rs:506`, `adapters/responses/inbound.rs:394`; discovery builds its outbound map
from scratch and never copies it). `slots/tests.rs::no_forward_site_relays_the_admin_session_cookie`
asserts all three sites drop it, and removing `"cookie"` from `RESERVED_SLOTS` turns it RED.
The strip is whole-header, so a benign caller cookie is dropped too — a recorded decision, not an
oversight. See [[m9-admin-surface-security]].

**How to apply:** when a new credential kind or accept slot lands, the edit is `from_state` +
`RESERVED_SLOTS`/`SHARED_SLOTS`, and the `slots/tests.rs` mirror table + source tripwire
(`every_header_producing_site_is_classified`) must be extended. That per-key-loop shape is now
caught — the tripwire matches any non-`tests.rs` file declaring `-> HeaderMap`/`-> Option<HeaderMap>`,
however it builds one. The remaining hole is narrower: a site that mutates a request in place and
never returns a `HeaderMap` is caught only by the extend-into-`headers_mut` pattern.
