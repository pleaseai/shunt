---
name: pr391-slot-enumeration
description: PR #391 (issue #363) src/auth/slots.rs — the single credential-slot enumeration; audit verdict and the one residual (admin session cookie is an unenumerated accept slot).
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

**Residual gap:** the accept-site enumeration in the module docs omits `admin::authenticate`'s
**session cookie** path — `shunt_admin_session` in the `cookie` header is a full-write admin
credential, and no forward site strips `cookie`, so all three relay it. Low exploitability
(`Path=/admin; SameSite=Strict`, so a browser will not send it to `/v1/messages`), but it is the
same class of gap the module exists to close. See [[m9-admin-surface-security]].

**How to apply:** when a new credential kind or accept slot lands, the edit is `from_state` +
`RESERVED_SLOTS`/`SHARED_SLOTS`, and the `slots/tests.rs` mirror table + source tripwire
(`every_bulk_header_forward_is_a_registered_site`) must be extended. The tripwire's known hole:
it does not catch a site that appends headers one at a time in a loop.
