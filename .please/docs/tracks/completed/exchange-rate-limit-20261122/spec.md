# Rate-limit the workload token exchange endpoint

> Track: exchange-rate-limit-20261122

## Overview

`POST /v1/oauth/token` lets a GitHub Actions workflow trade its OIDC assertion for a shunt
token (#913). Nothing limits how often it can be called:

- A caller with one valid assertion can exchange it again and again for the assertion's whole
  lifetime (about five minutes), and each exchange returns a new token valid for
  `max_ttl_seconds`.
- A caller with an invalid assertion can make shunt look up signing keys and check signatures
  without any limit. (Fetching an issuer's key set is already spaced at least a minute apart, so
  the unbounded cost is the per-request lookup and signature check.)

This track adds two limits to the endpoint:

- successful exchanges per verified identity;
- failed attempts per client address.

Both limits are on by default. Operators can tune them in config. A request over either limit
gets the endpoint's OAuth-shaped error with status `429` and a `Retry-After` header. Operators
who run the exchange behind a trusted proxy can have the client address taken from the
forwarding headers through the existing `[server.access_control] trust_forwarded_for` key,
which already decides the client address for this route's address allow-list.

Requested in #914, which was split out of #913 when rate limiting was deferred.

## Requirements

### Functional Requirements

- [ ] FR-1: Limit successful exchanges per verified identity. Allow at most `max` successful
  exchanges for one identity within a `window_seconds` window, configured by
  `[server.rate_limits] exchange`. The default is 60 per 600 seconds.
- [ ] FR-2: Limit failed exchange attempts per client address. Allow at most `max` failed
  attempts from one client address within a `window_seconds` window, configured by
  `[server.rate_limits] exchange_failure`. The default is 10 per 600 seconds. Once an address
  is over the limit, refuse its requests before verifying any assertion, so a blocked caller
  costs no signing-key lookups or signature checks. A failure is counted when it is answered, so
  requests from one address already in flight when it reaches the limit still complete and can
  take it past `max`; how many can be in flight is bounded only by `max_concurrent_requests`,
  and not at all when that is `0`. An
  atomic charge-and-refund was tried and reverted because it counted cancelled requests (AC-014)
  and refused concurrent valid exchanges (AC-006); see the plan's Decision Log.
- [ ] FR-3: Answer an over-limit request with HTTP `429`, carrying:
  - the endpoint's OAuth error shape (`{error, error_description}`) with a fixed error code and
    description;
  - a `Retry-After` header giving the whole seconds until the caller's window resets;
  - `cache-control: no-store`.

  Both limits use the same body, so a caller cannot tell which limit refused it or why its
  earlier attempts failed.
- [ ] FR-4: Accept `[server.rate_limits] exchange` and `exchange_failure` as optional tables
  with `max` and `window_seconds`, using the same per-field merge with defaults as
  `device_authorization` and `device_verify`. An omitted table or field takes its default. As
  with the existing tables, `max` and `window_seconds` must be greater than zero, so the limits
  cannot be switched off.
- [ ] FR-5: Derive the client address from the existing `[server.access_control] trust_forwarded_for`
  key (default `false`), adding no new key:
  - When `true`, take the client address from `X-Forwarded-For` (first entry) or `X-Real-IP`,
    the same way the `/device` limiter does.
  - Otherwise, use the TCP peer address.

  This works whether or not `[server.gateway]` is configured.
- [ ] FR-6: Document the new tables, their defaults, the `429` response, and the restart
  requirement for limit changes in the configuration reference (English and the ko/ja/zh-cn
  copies), the GitHub Actions OIDC guide, `shunt.toml.example`, and `docs/inbound-jwt-auth.md`.

### Non-functional Requirements

- [ ] NFR-1: Bound the limiter's memory the same way the existing gateway limiters are bounded:
  - the number of tracked identities and addresses is capped;
  - idle entries are swept.

  Counters stay in memory only and are not persisted.
- [ ] NFR-2: Keep every existing exchange response unchanged for callers under both limits:
  success body, `400`/`401`/`503` OAuth errors, and the `404` once a reload removes the table.
  The one exception is the capacity case in AC-013: when the failure limiter's table is full, an
  untracked address's failed attempts are answered `429` instead.

## Acceptance Criteria

- [ ] AC-001 (FR-1): When one identity has completed the configured maximum of successful
  exchanges within the window and presents another valid assertion, the system shall answer
  `429` and mint no token.
- [ ] AC-002 (FR-1): When the window for an identity that hit the success limit has elapsed,
  the system shall exchange that identity's next valid assertion normally.
- [ ] AC-003 (FR-1): When two different identities exchange from the same client address, the
  system shall count each identity's successful exchanges separately.
- [ ] AC-004 (FR-2): When one client address has made the configured maximum of failed
  attempts within the window and sends another request, the system shall answer `429` without
  verifying the assertion. This holds even if the assertion is valid.
- [ ] AC-005 (FR-2): When a request fails because the request has the wrong content type, the
  assertion is malformed, the request carries an unsupported grant type, or does not verify, the system shall count it against that client
  address's failure limit.
- [ ] AC-006 (FR-2): A successful exchange shall not count against the failure limit for that
  client address.
- [ ] AC-007 (FR-3): When the system refuses a request for being over either limit, the
  response shall carry a `Retry-After` header whose value is a positive whole number of
  seconds, no larger than the window.
- [ ] AC-008 (FR-3): The system shall not return a status, body, or header other than the
  `Retry-After` value from which the caller can tell the identity limit and the failure limit
  apart. (`Retry-After` follows each limit's own window, so it can differ when the two windows
  are configured differently.)
- [ ] AC-009 (FR-4): When `[server.rate_limits] exchange` sets only `max`, the system shall use
  that `max` with the default `window_seconds`.
- [ ] AC-010 (FR-5): While `[server.access_control] trust_forwarded_for` is `true`, when requests arrive through one
  proxy address with different `X-Forwarded-For` values, the system shall count failures per
  forwarded address.
- [ ] AC-011 (FR-5): While `[server.access_control] trust_forwarded_for` is `false`, if a request carries an
  `X-Forwarded-For` header, then the system shall ignore it and count failures against the TCP
  peer address.
- [ ] AC-012 (FR-1): If the identity limiter already tracks its maximum number of identities,
  then the system shall refuse a request from a new identity with `429` rather than grow without
  bound.
- [ ] AC-013 (FR-2): If the failure limiter already tracks its maximum number of addresses, then
  the system shall still exchange a valid assertion from an address it does not track, and shall
  answer that address's failed attempts with `429` without tracking it.
- [ ] AC-014 (FR-2): When a request fails because the signing keys for its issuer are temporarily
  unavailable, the system shall not count it against that client address's failure limit.

## Out of Scope

- Per-token revocation.
- Refusing a repeat exchange of the same assertion (replay). The identity limit caps how many
  tokens one identity can mint in a window; it does not stop one assertion from being exchanged
  again within its lifetime, or a later exchange from extending how long a token is held.
- Changes to the exchange token format.
- Persisting limiter counters across restarts.
- Applying changed limit values on hot reload. `[server.rate_limits]` already needs a restart
  and warns when it changes, so the new tables follow the same rule.
- Rate-limiting other endpoints.

## Assumptions

- **"Identity"** is the value the matching `[[server.auth.jwt]]` entry extracts through its
  identity claim. This is the same value the minted token carries as its subject. The plan
  decides whether to scope the counter by entry as well.
- **What counts as a failure.** Malformed requests, a wrong content type, an unsupported grant
  type, and assertions no opted-in entry accepts all count as failures. A temporary signing-key
  fetch outage (`503`) does not, because the caller did nothing wrong (AC-005, AC-014).
- **The `429` error code** is fixed, non-diagnostic, and shared by both limits. The plan picks
  the code. OAuth 2.0 defines no code specifically for rate limiting.
- **Defaults** come from the user's choice of "on by default". The failure limit matches the
  existing `device_verify` limit (10 per 600 seconds). The success limit (60 per 600 seconds)
  leaves room for matrix workflows whose jobs share one identity.
- **`[server.access_control] trust_forwarded_for`** keeps its existing semantics (the same
  header precedence as the `/device` limiter). It should be enabled only behind a trusted proxy that overwrites client-supplied
  values.
