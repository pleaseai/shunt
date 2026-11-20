---
product_spec_domain: auth/inbound-jwt
---

# Inbound JWT from a configured header or cookie for Codex gateway OAuth

> Track: inbound-jwt-header-cookie-20261030
> Issue: pleaseai/shunt#860 (related: #660, builds on #344)

## Overview

The Codex CLI can sign each developer in to an organization's model gateway through the
organization's identity provider (native gateway OAuth). Codex reserves the `Authorization`
header for its own use and delivers the identity-provider access token in either a named
**cookie** or a named **custom header**. shunt verifies identity-provider JWTs today, but reads
them only from `Authorization: Bearer`, so a Codex CLI using gateway OAuth is rejected on every
request.

This track lets an operator name the header and/or cookie that carries the identity-provider
JWT. The inbound Codex endpoint then verifies a token found there with the existing
`[[server.auth.jwt]]` rules, and never forwards it upstream. shunt still issues nothing: the
identity provider owns sign-in, token lifetime, and revocation. The result is per-user Codex
sign-in, with per-user attribution and spend limits, for organizations that already run an
identity provider such as Microsoft Entra ID.

Decisions recorded for this track:

- The two keys live on `[server.auth]` as `jwt_header` and `jwt_cookie`, next to the
  `[[server.auth.jwt]]` entries they feed.
- Only the inbound Codex endpoint's routes read the new slots. Every other gated route keeps
  accepting a JWT in `Authorization: Bearer` only.
- Identity-provider pitfalls (Microsoft Entra ID) are covered in the documentation only; no new
  runtime checks or warnings.

## Requirements

### Functional Requirements

- [ ] FR-1: accept an optional `[server.auth] jwt_header` key naming a request header from which an inbound JWT is read, stripping a leading `Bearer ` scheme when present
- [ ] FR-2: accept an optional `[server.auth] jwt_cookie` key naming a cookie from which an inbound JWT is read
- [ ] FR-3: verify a token found in a configured header or cookie against the configured `[[server.auth.jwt]]` entries with exactly the rules applied to a bearer JWT (issuer, audience, authorized party, token age, claim rules, identity claim)
- [ ] FR-4: read the configured header and cookie on the inbound Codex endpoint's routes only — Responses over HTTP and WebSocket on all three base paths, web search, and the client analytics sink — and nowhere else
- [ ] FR-5: try the credential slots in a fixed order — static token, then `Authorization: Bearer` JWT, then the configured header, then the configured cookie — and authenticate the caller with the first slot that succeeds
- [ ] FR-6: use the verified identity from a header or cookie token as the caller principal for usage attribution, spend limits, and account-pool session stickiness, exactly as a bearer JWT identity is used
- [ ] FR-7: remove the configured header and every `cookie` header from the request before it is forwarded to any upstream, on both HTTP and WebSocket
- [ ] FR-8: reject at startup a configuration that sets `jwt_header` or `jwt_cookie` without any `[[server.auth.jwt]]` entry, a `jwt_header` that is not a valid header name, a `jwt_header` equal (case-insensitively) to `authorization`, `x-api-key`, `cookie`, or a shunt-reserved header name (`x-shunt-token`, `x-shunt-admin-token`, `x-shunt-inbound-client`), a `jwt_header` naming a header shunt itself reads or rewrites on the Codex endpoint (`session-id`, `x-claude-code-session-id`, `content-type`, `content-length`, `content-encoding`, `transfer-encoding`, `host`, `origin`, `upgrade`, `connection`, `openai-beta`, `sec-websocket-*`) or that the Codex CLI sends or shunt synthesizes there (`originator`, `x-codex-routing-hint`, `thread-id`, `x-client-request-id`, `x-codex-window-id`), a `jwt_header` equal to the `[server.auth] header` or `[server.admin] header` name, and a `jwt_cookie` that is empty, not a valid cookie name, or equal (case-insensitively) to the admin dashboard's `shunt_admin_session` cookie
- [ ] FR-9: keep gateway-owned authentication failures on the Codex endpoint in the existing OpenAI Responses error shape and status codes — one undifferentiated `401` for a missing or rejected credential, `503` when no slot authenticates and at least one presented token names a configured issuer whose key set cannot be fetched
- [ ] FR-10: apply configuration reloads to `jwt_header` and `jwt_cookie` the same way reloads already apply to `[server.auth]`
- [ ] FR-11: document the keys and the Codex gateway OAuth setup in the configuration reference, the inbound JWT design note, and the Codex connection guide (English plus the `ko`, `ja`, and `zh-cn` site copies), including a Microsoft Entra ID example and its pitfalls: JWT-only access tokens and the API scope request, token lifetime versus `max_token_age_seconds`, `azp` versus `authorized_parties` and v2 tokens, missing `email_verified`, and the fixed redirect port — plus a note that `jwt_cookie` must not name a cookie a browser holds for the gateway's domain (for example one set by a front-end SSO proxy), since a browser attaches such a cookie to cross-site requests

- [ ] FR-12: consult the configured cookie only when the request carries no `Origin` header or one whose origin matches the request's scheme (`X-Forwarded-Proto`, else `http`), `Host`, and effective port, so a browser holding that cookie for the gateway's domain cannot be driven by another site to run authenticated turns

### Non-functional Requirements

- [ ] NFR-1: leave behavior unchanged for every deployment that sets neither key — same accepted credentials, same headers forwarded, same responses
- [ ] NFR-2: avoid disclosing which slot or which check failed in an authentication error or in logs at default verbosity, and never log a token value

## Acceptance Criteria

- [ ] AC-001 (FR-1, FR-4): When a Codex request to `/v1/responses` carries a valid identity-provider JWT in the header named by `jwt_header` and no `Authorization` header, the system shall authenticate the caller and serve the turn.
- [ ] AC-002 (FR-1): When the configured header's value is `Bearer <jwt>`, the system shall verify `<jwt>` without the scheme prefix.
- [ ] AC-003 (FR-2, FR-4): When a Codex WebSocket upgrade on `/backend-api/codex/responses` carries a valid JWT in the cookie named by `jwt_cookie` among other cookies, the system shall authenticate the upgrade before switching protocols.
- [ ] AC-004 (FR-5): When `Authorization` carries a ChatGPT sign-in token from an issuer no `[[server.auth.jwt]]` entry names and the configured cookie carries a valid JWT, the system shall authenticate the caller from the cookie.
- [ ] AC-005 (FR-2): If a request carries the configured cookie name more than once, counted across every `cookie` header field, and no earlier slot authenticates the caller, then the system shall reject the request with `401`.
- [ ] AC-016 (FR-1): If a request carries the configured header more than once and no earlier slot authenticates the caller, then the system shall reject the request with `401`.
- [ ] AC-017 (FR-12): If a WebSocket upgrade carries a valid JWT only in the configured cookie and an `Origin` whose host differs from the request's `Host`, then the system shall reject the upgrade with `401`.
- [ ] AC-006 (FR-3): If the token in the configured header was issued for a different audience, then the system shall reject the request with `401`.
- [ ] AC-007 (FR-4): When a request to `/v1/messages` carries a valid JWT only in the configured header or cookie, the system shall reject it as unauthenticated.
- [ ] AC-008 (FR-6): When a caller authenticated from the configured cookie is over its spend cap, the system shall refuse the turn with `usage_limit_reached` under that caller's identity.
- [ ] AC-009 (FR-7): When a request authenticated from the configured header is forwarded to the ChatGPT/Codex account pool, the upstream request shall contain neither the configured header nor any `cookie` header.
- [ ] AC-010 (FR-7): When a Codex request is served by a `[[server.codex_endpoint.routes]]` route to a non-ChatGPT upstream, the upstream request shall contain neither the configured header nor any `cookie` header.
- [ ] AC-011 (FR-8): If `jwt_cookie` is set and no `[[server.auth.jwt]]` entry exists, then the system shall refuse to start and name the offending key.
- [ ] AC-012 (FR-8): If `jwt_header` equals the `[server.admin] header` name, then the system shall refuse to start.
- [ ] AC-013 (FR-9): If the configured header carries a token from a configured issuer whose key set cannot be fetched and no other slot authenticates, then the system shall respond `503` in the OpenAI Responses error shape.
- [ ] AC-014 (FR-10): When a reload adds `jwt_header` to a running configuration, the system shall accept a JWT from that header on the next Codex request without a restart.
- [ ] AC-015 (NFR-1): While neither `jwt_header` nor `jwt_cookie` is set, the system shall accept and forward exactly the credentials and headers it does today.

## Out of Scope

- shunt acting as an OAuth authorization server for Codex (authorization endpoint, `authorization_code` with PKCE, issued gateway tokens) — tracked in #660.
- Reading the new slots on any route outside the inbound Codex endpoint, including `/v1/messages`, `/v1/models`, `/usage`, and the Codex discovery paths `/models` and `/backend-api/codex/models`.
- Forwarding the caller's own ChatGPT credential to the ChatGPT backend (the "recommended" ChatGPT sign-in route in OpenAI's gateway guide); the endpoint keeps injecting pool accounts.
- Token introspection or support for opaque (non-JWT) access tokens.
- Runtime or startup warnings for identity-provider misconfiguration.
- Changes to `[server.gateway]` device login or to `[[server.auth.jwt]]` verification rules themselves.

## Assumptions

- The Codex CLI sends the gateway token on every Codex endpoint request it makes, including WebSocket upgrades, web search, and analytics posts; the issue's acceptance criteria cover the Responses routes, and search and analytics are included because they share the same gate today.
- With gateway OAuth as its only credential, the Codex CLI does not fetch a remote model catalog (per OpenAI's guide: "OAuth alone doesn't enable remote catalog discovery"), so excluding the discovery paths does not break a gateway-OAuth client.
- Stripping every `cookie` header on forward is acceptable because the Codex endpoint already strips it today and no upstream depends on cookies.
- Rejecting a duplicated configured cookie (rather than picking one) is the safe default, since two values for one credential name indicate injection or a misbehaving client.
- Microsoft Entra ID details in the docs (60–90 minute default access-token lifetime, `azp` on v2 tokens, no `email_verified` claim) reflect Entra behavior as of the issue; the guide should link Microsoft's documentation rather than restate figures beyond what is needed.
