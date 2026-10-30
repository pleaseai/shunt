---
id: SPEC-003
level: V_M
domain: auth
feature: inbound-jwt
depends: []
conflicts: []
traces: []
created_at: 2026-10-30T03:04:00.075Z
updated_at: 2026-10-30T03:04:00.075Z
source_tracks: ["inbound-jwt-header-cookie-20261030"]
---

# Inbound jwt Specification

## Purpose

Requirements for accepting inbound JWTs from a configured header or cookie on the Codex endpoint.

## Requirements

### Requirement: accept an optional [server.auth] jwt_header key naming a request header from which an inbound JWT is read, stripping a leading Bearer scheme when present
<!-- req: REQ-001 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST accept an optional [server.auth] jwt_header key naming a request header from which an inbound JWT is read, stripping a leading Bearer scheme when present.

#### Scenario: When a Codex request to /v1/responses carries a valid identity-provider JWT in the header named by jwt_header and no Authorization header, the system shall authenticate the caller and serve the turn.

- GIVEN jwt_header names a header
- WHEN a Codex request to /v1/responses carries a valid identity-provider JWT in that header and no Authorization header
- THEN the system authenticates the caller and serves the turn

#### Scenario: When the configured header's value is Bearer <jwt>, the system shall verify <jwt> without the scheme prefix.

- WHEN the configured header's value is Bearer <jwt>
- THEN the system verifies <jwt> without the scheme prefix

#### Scenario: If a request carries the configured header more than once and no earlier slot authenticates the caller, then the system shall reject the request with 401.

- GIVEN no earlier slot authenticates the caller
- WHEN a request carries the configured header more than once
- THEN the system rejects the request with 401

### Requirement: accept an optional [server.auth] jwt_cookie key naming a cookie from which an inbound JWT is read
<!-- req: REQ-002 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST accept an optional [server.auth] jwt_cookie key naming a cookie from which an inbound JWT is read.

#### Scenario: When a Codex WebSocket upgrade on /backend-api/codex/responses carries a valid JWT in the cookie named by jwt_cookie among other cookies, the system shall authenticate the upgrade before switching protocols.

- GIVEN jwt_cookie names a cookie
- WHEN a Codex WebSocket upgrade on /backend-api/codex/responses carries a valid JWT in that cookie among other cookies
- THEN the system authenticates the upgrade before switching protocols

#### Scenario: If a request carries the configured cookie name more than once, counted across every cookie header field, and no earlier slot authenticates the caller, then the system shall reject the request with 401.

- GIVEN no earlier slot authenticates the caller
- WHEN a request carries the configured cookie name more than once across its cookie header fields
- THEN the system rejects the request with 401

### Requirement: verify a token found in a configured header or cookie against the configured [[server.auth.jwt]] entries with exactly the rules applied to a bearer JWT (issuer, audience, authorized party, token age, claim rules, identity claim)
<!-- req: REQ-003 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST verify a token found in a configured header or cookie against the configured [[server.auth.jwt]] entries with exactly the rules applied to a bearer JWT (issuer, audience, authorized party, token age, claim rules, identity claim).

#### Scenario: If the token in the configured header was issued for a different audience, then the system shall reject the request with 401.

- WHEN the token in the configured header was issued for a different audience
- THEN the system rejects the request with 401

### Requirement: read the configured header and cookie on the inbound Codex endpoint's routes only — Responses over HTTP and WebSocket on all three base paths, web search, and the client analytics sink — and nowhere else
<!-- req: REQ-004 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST read the configured header and cookie on the inbound Codex endpoint's routes only — Responses over HTTP and WebSocket on all three base paths, web search, and the client analytics sink — and nowhere else.

#### Scenario: When a Codex request to /v1/responses carries a valid identity-provider JWT in the header named by jwt_header and no Authorization header, the system shall authenticate the caller and serve the turn.

- GIVEN jwt_header names a header
- WHEN a Codex request to /v1/responses carries a valid identity-provider JWT in that header and no Authorization header
- THEN the system authenticates the caller and serves the turn

#### Scenario: When a Codex WebSocket upgrade on /backend-api/codex/responses carries a valid JWT in the cookie named by jwt_cookie among other cookies, the system shall authenticate the upgrade before switching protocols.

- GIVEN jwt_cookie names a cookie
- WHEN a Codex WebSocket upgrade on /backend-api/codex/responses carries a valid JWT in that cookie among other cookies
- THEN the system authenticates the upgrade before switching protocols

#### Scenario: When a request to /v1/messages carries a valid JWT only in the configured header or cookie, the system shall reject it as unauthenticated.

- WHEN a request to /v1/messages carries a valid JWT only in the configured header or cookie
- THEN the system rejects it as unauthenticated

### Requirement: try the credential slots in a fixed order — static token, then Authorization: Bearer JWT, then the configured header, then the configured cookie — and authenticate the caller with the first slot that succeeds
<!-- req: REQ-005 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST try the credential slots in a fixed order — static token, then Authorization: Bearer JWT, then the configured header, then the configured cookie — and authenticate the caller with the first slot that succeeds.

#### Scenario: When Authorization carries a ChatGPT sign-in token from an issuer no [[server.auth.jwt]] entry names and the configured cookie carries a valid JWT, the system shall authenticate the caller from the cookie.

- WHEN Authorization carries a ChatGPT sign-in token from an unconfigured issuer and the configured cookie carries a valid JWT
- THEN the system authenticates the caller from the cookie

### Requirement: use the verified identity from a header or cookie token as the caller principal for usage attribution, spend limits, and account-pool session stickiness, exactly as a bearer JWT identity is used
<!-- req: REQ-006 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST use the verified identity from a header or cookie token as the caller principal for usage attribution, spend limits, and account-pool session stickiness, exactly as a bearer JWT identity is used.

#### Scenario: When a caller authenticated from the configured cookie is over its spend cap, the system shall refuse the turn with usage_limit_reached under that caller's identity.

- GIVEN the caller authenticated from the configured cookie is over its spend cap
- WHEN the caller requests a turn
- THEN the system refuses the turn with usage_limit_reached under that caller's identity

### Requirement: remove the configured header and every cookie header from the request before it is forwarded to any upstream, on both HTTP and WebSocket
<!-- req: REQ-007 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST remove the configured header and every cookie header from the request before it is forwarded to any upstream, on both HTTP and WebSocket.

#### Scenario: When a request authenticated from the configured header is forwarded to the ChatGPT/Codex account pool, the upstream request shall contain neither the configured header nor any cookie header.

- WHEN a request authenticated from the configured header is forwarded to the ChatGPT/Codex account pool
- THEN the upstream request contains neither the configured header nor any cookie header

#### Scenario: When a Codex request is served by a [[server.codex_endpoint.routes]] route to a non-ChatGPT upstream, the upstream request shall contain neither the configured header nor any cookie header.

- WHEN a Codex request is served by a [[server.codex_endpoint.routes]] route to a non-ChatGPT upstream
- THEN the upstream request contains neither the configured header nor any cookie header

### Requirement: reject at startup a configuration that sets jwt_header or jwt_cookie without any [[server.auth.jwt]] entry, a jwt_header that is not a valid header name, equals (case-insensitively) authorization, x-api-key, cookie, a shunt-reserved header name, a header shunt itself reads or rewrites on the Codex endpoint (session-id, x-claude-code-session-id, content-type, content-length, content-encoding, transfer-encoding, host, origin, upgrade, connection, openai-beta, sec-websocket-*) or that the Codex CLI sends or shunt synthesizes there (originator, x-codex-routing-hint, thread-id, x-client-request-id, x-codex-window-id), or the [server.auth] or [server.admin] header name, and a jwt_cookie that is empty, not a valid cookie name, or equal (case-insensitively) to the admin dashboard's shunt_admin_session cookie
<!-- req: REQ-008 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST reject at startup a configuration that sets jwt_header or jwt_cookie without any [[server.auth.jwt]] entry, a jwt_header that is not a valid header name, equals (case-insensitively) authorization, x-api-key, cookie, a shunt-reserved header name, a header shunt itself reads or rewrites on the Codex endpoint (session-id, x-claude-code-session-id, content-type, content-length, content-encoding, transfer-encoding, host, origin, upgrade, connection, openai-beta, sec-websocket-*) or that the Codex CLI sends or shunt synthesizes there (originator, x-codex-routing-hint, thread-id, x-client-request-id, x-codex-window-id), or the [server.auth] or [server.admin] header name, and a jwt_cookie that is empty, not a valid cookie name, or equal (case-insensitively) to the admin dashboard's shunt_admin_session cookie.

#### Scenario: If jwt_cookie is set and no [[server.auth.jwt]] entry exists, then the system shall refuse to start and name the offending key.

- GIVEN no [[server.auth.jwt]] entry exists
- WHEN jwt_cookie is set
- THEN the system refuses to start and names the offending key

#### Scenario: If jwt_header equals the [server.admin] header name, then the system shall refuse to start.

- WHEN jwt_header equals the [server.admin] header name
- THEN the system refuses to start

### Requirement: keep gateway-owned authentication failures on the Codex endpoint in the OpenAI Responses error shape and status codes — one undifferentiated 401 for a missing or rejected credential, 503 when no slot authenticates and at least one presented token names a configured issuer whose key set cannot be fetched
<!-- req: REQ-009 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST keep gateway-owned authentication failures on the Codex endpoint in the OpenAI Responses error shape and status codes — one undifferentiated 401 for a missing or rejected credential, 503 when no slot authenticates and at least one presented token names a configured issuer whose key set cannot be fetched.

#### Scenario: If the configured header carries a token from a configured issuer whose key set cannot be fetched and no other slot authenticates, then the system shall respond 503 in the OpenAI Responses error shape.

- GIVEN no other slot authenticates
- WHEN the configured header carries a token from a configured issuer whose key set cannot be fetched
- THEN the system responds 503 in the OpenAI Responses error shape

### Requirement: apply configuration reloads to jwt_header and jwt_cookie the same way reloads already apply to [server.auth]
<!-- req: REQ-010 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST apply configuration reloads to jwt_header and jwt_cookie the same way reloads already apply to [server.auth].

#### Scenario: When a reload adds jwt_header to a running configuration, the system shall accept a JWT from that header on the next Codex request without a restart.

- GIVEN shunt is running
- WHEN a reload adds jwt_header
- THEN the system accepts a JWT from that header on the next Codex request without a restart

### Requirement: consult the configured cookie only when the request carries no Origin header or one whose origin matches the request's scheme (X-Forwarded-Proto, else http), Host, and effective port
<!-- req: REQ-011 tracks=inbound-jwt-header-cookie-20261030 -->

The system MUST consult the configured cookie only when the request carries no Origin header or one whose origin matches the request's scheme (X-Forwarded-Proto, else http), Host, and effective port.

#### Scenario: If a WebSocket upgrade carries a valid JWT only in the configured cookie and an Origin whose host differs from the request's Host, then the system shall reject the upgrade with 401.

- WHEN a WebSocket upgrade carries a valid JWT only in the configured cookie and an Origin whose host differs from the request's Host
- THEN the system rejects the upgrade with 401

## Non-functional Requirements

### Requirement: leave behavior unchanged for every deployment that sets neither jwt_header nor jwt_cookie — same accepted credentials, same headers forwarded, same responses
<!-- req: REQ-012 tracks=inbound-jwt-header-cookie-20261030 -->

The system SHOULD leave behavior unchanged for every deployment that sets neither jwt_header nor jwt_cookie — same accepted credentials, same headers forwarded, same responses.

### Requirement: avoid disclosing which credential slot or which check failed in an authentication error or in logs at default verbosity, and never log a token value
<!-- req: REQ-013 tracks=inbound-jwt-header-cookie-20261030 -->

The system SHOULD avoid disclosing which credential slot or which check failed in an authentication error or in logs at default verbosity, and never log a token value.
