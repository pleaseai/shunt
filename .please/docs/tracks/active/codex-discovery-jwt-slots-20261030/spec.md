---
product_spec_domain: auth/inbound-jwt
---

# Codex model discovery accepts the gateway OAuth header and cookie

> Track: codex-discovery-jwt-slots-20261030
> Builds on: pleaseai/shunt#860 (PR #862) · Related: #660

## Overview

A Codex CLI user can sign in to an organization's gateway with the organization's identity
provider (native gateway OAuth) and, at the same time, keep ChatGPT sign-in on. In that setup
Codex sends the ChatGPT token in `Authorization` and the identity-provider token in a configured
cookie or header. Since #862, shunt accepts that cookie or header on the inbound Codex endpoint's
Responses, web search, and analytics routes, but not on model discovery. Codex fetches its model
catalog only when ChatGPT sign-in is on, so every catalog fetch from such a user is rejected with
`401`; Codex logs an error and falls back to its bundled catalog.

This track lets the Codex-shaped model discovery responses accept the token from the configured
`jwt_header` / `jwt_cookie` slot, with the same rules the Codex endpoint already applies. A
signed-in gateway OAuth user then gets the same discovery answer as a client using a static
token. The answer itself stays the empty Codex catalog shunt serves today; serving real model
records is a separate follow-up.

## Requirements

### Functional Requirements

- [ ] FR-1: accept a token from the `[server.auth]` `jwt_header` and `jwt_cookie` slots on every Codex-shaped model discovery response — `GET /models`, `GET /backend-api/codex/models`, and `GET /v1/models` when `[server.codex_endpoint]` is enabled and the request carries the `client_version` query field
- [ ] FR-2: verify a token found in either slot with the configured `[[server.auth.jwt]]` entries and the same slot order, `Origin` rule for the cookie, and repeated-slot handling the inbound Codex endpoint applies
- [ ] FR-3: keep accepting, on the Codex-shaped discovery responses, every credential they accept today — static tokens in the configured `[server.auth]` header, `x-api-key`, or `Authorization: Bearer`, a JWT in `Authorization: Bearer`, and a `[server.gateway]` login token
- [ ] FR-4: keep the Anthropic-shaped `GET /v1/models` response (no `client_version` query field, or `[server.codex_endpoint]` not enabled) from reading the `jwt_header` and `jwt_cookie` slots
- [ ] FR-5: answer an authentication failure on a Codex-shaped discovery response in the OpenAI error shape — one undifferentiated `401` for a missing or rejected credential, `503` when no credential authenticates and a presented token names a configured issuer whose key set cannot be fetched
- [ ] FR-6: document that Codex model discovery accepts the configured header and cookie, in the inbound JWT design note, the inbound Codex endpoint milestone note, and the site's Codex CLI connection guide, inbound Codex endpoint guide, and configuration reference — each site page in English plus its `ko`, `ja`, and `zh-cn` copies — replacing the current statements that the Codex discovery paths accept a JWT only in `Authorization: Bearer`

### Non-functional Requirements

- [ ] NFR-1: leave behavior unchanged for every deployment that sets neither `jwt_header` nor `jwt_cookie` — same accepted credentials and same responses on every discovery path
- [ ] NFR-2: avoid disclosing which slot or which check failed in an authentication error or in logs at default verbosity, and never log a token value

## Acceptance Criteria

- [ ] AC-001 (FR-1): When a request to `GET /v1/models?client_version=<v>` carries a ChatGPT token in `Authorization` and a valid identity-provider JWT in the cookie named by `jwt_cookie`, the system shall answer `200` with the Codex catalog envelope.
- [ ] AC-002 (FR-1): When a request to `GET /backend-api/codex/models` carries a valid identity-provider JWT only in the header named by `jwt_header`, as `Bearer <jwt>`, the system shall answer `200` with the Codex catalog envelope.
- [ ] AC-003 (FR-2): If a request to `GET /models` carries a valid JWT only in the configured cookie and an `Origin` whose host differs from the request's `Host`, then the system shall answer `401`.
- [ ] AC-004 (FR-2): If a request to `GET /models` carries the configured header twice and no earlier credential authenticates the caller, then the system shall answer `401`.
- [ ] AC-005 (FR-2): If the token in the configured header was issued for a different audience, then the system shall answer `401` on a Codex-shaped discovery request.
- [ ] AC-006 (FR-3): When `jwt_header` is set and a request to `GET /models` carries a valid static token in `x-api-key`, the system shall answer `200`.
- [ ] AC-007 (FR-4): If a request to `GET /v1/models` without a `client_version` query field carries a valid JWT only in the configured cookie or header, then the system shall answer `401`.
- [ ] AC-008 (FR-5): If the configured cookie carries a token from a configured issuer whose key set cannot be fetched and no other credential authenticates, then the system shall answer `503` in the OpenAI error shape on a Codex-shaped discovery request.
- [ ] AC-009 (FR-5): When a request to `GET /models` carries a valid `[server.gateway]` login token in `Authorization: Bearer` and, in the configured cookie, a token from a configured issuer whose key set cannot be fetched, the system shall answer `200` with the Codex catalog envelope.

## Out of Scope

- **Follow-up: a real Codex model catalog.** shunt keeps serving the empty Codex catalog (`{"models":[]}`). Building real Codex model records from the configured routes, so Codex lists the models shunt serves, is a separate track.
- Forwarding the caller's own ChatGPT credential to the ChatGPT backend.
- Reading the `jwt_header` / `jwt_cookie` slots on `/v1/messages`, `/v1/messages/count_tokens`, `/usage`, `/api/oauth/usage`, or the Anthropic-shaped `/v1/models` response.
- Token introspection or opaque (non-JWT) access tokens.
- shunt acting as an OAuth authorization server for Codex (#660).
- Changes to `[[server.auth.jwt]]` verification rules, the slot configuration keys, or their startup validation.

## Assumptions

- Codex sends the gateway OAuth cookie or header on its catalog request and fetches the catalog only when ChatGPT sign-in (or command or API-key auth) is active; with gateway OAuth alone it does not fetch. This follows the current `openai/codex` source (`model-provider/src/combined_auth.rs`, `models-manager/src/manager.rs`).
- A rejected catalog request is not fatal in Codex (it logs and keeps its bundled catalog), so the user-visible gain is a consistent answer across the Codex routes and no error on each catalog refresh, not a different model list.
- Reading the cookie on `GET /v1/models?client_version=…` is safe even though the query field is caller-chosen: the Codex-shaped answer is the fixed empty catalog and discloses nothing, and the `Origin` rule still applies.
- Discovery forwards nothing upstream, so no header or cookie stripping is involved.
</content>
</invoke>
