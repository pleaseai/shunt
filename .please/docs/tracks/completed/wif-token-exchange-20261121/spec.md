# Workload token exchange for GitHub Agentic Workflows

> Track: wif-token-exchange-20261121
> Issue: pleaseai/shunt#913

## Overview

GitHub Agentic Workflows (gh-aw) runs Claude Code inside the AWF firewall sandbox. There, the
credential is held by the AWF api-proxy sidecar, not by the agent. That rules out every way a
workflow can authenticate to shunt with GitHub Actions OIDC today. A pre-minted OIDC token expires
after about five minutes, while AWF treats a static key as fixed for the whole run. The agent
container cannot mint fresh tokens, because the Actions token-request variables are withheld from
it. And AWF's static-key path sends `x-api-key`, while shunt reads JWTs only from
`Authorization: Bearer`.

gh-aw offers one keyless option for Claude: Anthropic Workload Identity Federation
(`engine.auth.type: github-oidc`, `provider: anthropic`), with a configurable token endpoint. The
sidecar mints the GitHub OIDC token, trades it at that endpoint for an access token, caches the
access token, and refreshes it before expiry.

This track makes shunt that token endpoint. A shunt operator who already accepts GitHub Actions
OIDC tokens through `[[server.auth.jwt]]` can opt individual workload entries into the exchange.
A gh-aw workflow then reaches shunt with no static secret, for runs of any length. Each run is
attributed and spend-limited as the same principal it would be under direct JWT use.

The exchange is strictly a lifetime extension of an identity shunt already trusts. It never
authorizes a caller that direct use of the same OIDC token would not authorize.

## Requirements

### Functional Requirements

- [ ] FR-1: Accept a token-exchange request at `POST /v1/oauth/token` using the JSON request shape
      the AWF sidecar sends: `grant_type` = `urn:ietf:params:oauth:grant-type:jwt-bearer`, an
      `assertion` holding the GitHub Actions OIDC JWT, and the four Anthropic federation ids
      (`federation_rule_id`, `organization_id`, `service_account_id`, `workspace_id`).
- [ ] FR-2: Verify the `assertion` against the configured `[[server.auth.jwt]]` entries that opt
      into the exchange. The checks are exactly those applied to the same JWT on
      `Authorization: Bearer`: issuer selection, signature, audience, expiry and not-before, maximum
      token age, required claims, subject prefixes, authorized parties, email rules, and the
      identity claim.
- [ ] FR-3: Let an operator enable the exchange with a top-level `[server.auth.exchange]` table.
      The table holds a dedicated signing secret list (the first entry signs, every entry
      verifies, so the secret can be rotated) and a maximum issued-token lifetime
      (`max_ttl_seconds`, default 3600). Let the operator opt an individual `[[server.auth.jwt]]`
      entry into the exchange with `exchange = true` (default `false`).
- [ ] FR-4: On a successful exchange, issue a shunt-signed access token and answer `200` with
      `access_token`, `token_type` = `Bearer`, and `expires_in` (seconds).
- [ ] FR-5: Make the issued token carry its own token-use marker, distinct from the gateway-login
      session marker. Its caller identity is the identity-claim value resolved by the entry that
      verified the assertion, and it records which entry verified it, in a form that changes when
      that entry's verification rules change.
- [ ] FR-6: Set the issued token's lifetime to `max_ttl_seconds`, independent of the assertion's
      own expiry.
- [ ] FR-7: Accept the issued token in the `Authorization: Bearer` slot on every route that accepts
      a `[[server.auth.jwt]]` JWT there (`/v1/messages`, `/v1/messages/count_tokens`, model
      discovery, client usage, and the inbound Codex endpoint), resolving it to the same caller
      identity, usage attribution, and spend-limit principal as direct use of the verifying
      entry's JWT.
- [ ] FR-8: Accept an issued token only while the current configuration still has the entry that
      verified the assertion, with `exchange = true` and its verification rules unchanged, and only
      while one of the current signing secrets verifies it. Removing the entry, clearing its
      opt-in, changing any of its verification rules (audience, required claims, subject prefixes,
      authorized parties, email rules, identity claim), or removing every signing secret that
      verified it (including through a hot reload) stops shunt from accepting tokens already
      issued.
- [ ] FR-9: Ignore the four Anthropic federation ids: accept a request that carries them, and
      accept a request that omits them.
- [ ] FR-10: Answer a failed exchange in the OAuth error shape (`error`, `error_description`):
      `400` `invalid_request` for a malformed or incomplete body, `400` `unsupported_grant_type`
      for any other grant type, `401` `invalid_grant` for an assertion that no opted-in entry
      verifies, and `503` `temporarily_unavailable` when a matching issuer's key set cannot be
      fetched.
- [ ] FR-11: Strip the issued token from the shared credential slots before any request leaves
      shunt for an upstream. This covers passthrough routes and tokens that are expired, signed by
      a rotated-out secret, bound to a removed entry, or issued before `[server.auth.exchange]`
      was removed.
- [ ] FR-12: Validate the exchange configuration at load time and in `shunt check`. Reject a
      signing secret shorter than 32 bytes, a signing secret that is also a `[server.gateway]`
      session signing secret, an empty signing secret list, a zero
      `max_ttl_seconds`, an `exchange = true` entry when `[server.auth.exchange]` is absent, and a
      `[server.auth.exchange]` table that no entry opts into.
- [ ] FR-13: Document the exchange: a gh-aw section in `docs/inbound-jwt-auth.md` with a working
      workflow frontmatter example and the matching shunt config, plus the config and endpoint
      reference pages on the site and their ko/ja/zh-cn copies, the README capability row for
      inbound JWT (which today says shunt issues nothing) and its ko/ja/zh-CN copies, and the
      OAuth error-shape exception in AGENTS.md.

### Non-functional Requirements

- [ ] NFR-1: The exchange never widens access: every assertion it accepts would also be accepted
      as a direct Bearer JWT by the same entry, and no credential other than a `[[server.auth.jwt]]`
      JWT can be exchanged.
- [ ] NFR-2: Neither the assertion nor the issued token appears in logs, metrics labels, or error
      descriptions.
- [ ] NFR-3: A failed exchange does not tell the caller which check rejected the assertion. Every
      verification failure produces the same `401 invalid_grant` response.
- [ ] NFR-4: Deployments without `[server.auth.exchange]` behave exactly as before: the route does
      not exist (`404`), and no other route reads anything new. The FR-11 strip still applies, so
      a token issued before the table was removed is never forwarded upstream.

## Acceptance Criteria

- [ ] AC-001 (FR-1, FR-4): When the sidecar posts a valid GitHub Actions OIDC assertion that an
      opted-in entry verifies, the system shall answer `200` with a non-empty `access_token`,
      `token_type` `Bearer`, and `expires_in` equal to the configured `max_ttl_seconds`.
- [ ] AC-002 (FR-6): When the assertion has fewer than 60 seconds left before its expiry, the
      system shall still issue a token whose `expires_in` equals `max_ttl_seconds`.
- [ ] AC-003 (FR-7): When a client sends an issued token as `Authorization: Bearer` on
      `/v1/messages` with the `anthropic-beta: oauth-2025-04-20` header, the system shall
      authenticate the request as the verifying entry's identity and route it as it would route a
      request from a directly presented JWT.
- [ ] AC-004 (FR-7): When an issued token authenticates a gated `/v1/messages` request, the system
      shall record usage and enforce spend limits under the same principal that direct use of the
      verifying entry's JWT produces (for example `org/repo` for `identity_claim = "repository"`).
- [ ] AC-005 (FR-2, NFR-1): If the assertion fails any check that would reject it on
      `Authorization: Bearer` (wrong audience, expired, too old, missing required claim, bad
      signature, unknown `kid`), then the system shall answer `401` `invalid_grant`.
- [ ] AC-006 (FR-3, NFR-1): If the assertion verifies only against an entry without
      `exchange = true`, then the system shall answer `401` `invalid_grant`.
- [ ] AC-007 (FR-3): When the assertion's issuer has two entries with different audiences and only
      the second opts into the exchange, the system shall issue a token if the second entry
      verifies the assertion.
- [ ] AC-008 (NFR-1): If the assertion is a static `[server.auth]` client token, a gateway-login
      session token, or a previously issued exchange token, then the system shall answer `401`
      `invalid_grant`.
- [ ] AC-009 (FR-10): If `grant_type` is any value other than the jwt-bearer URN, then the system
      shall answer `400` `unsupported_grant_type`.
- [ ] AC-010 (FR-10): If the body is not JSON, or `assertion` is missing or empty, then the system
      shall answer `400` `invalid_request`.
- [ ] AC-011 (FR-10): If the key set of the assertion's matching issuer cannot be fetched, then the
      system shall answer `503` in the OAuth error shape rather than `401`.
- [ ] AC-012 (FR-9): When the request omits all four Anthropic federation ids, the system shall
      process the exchange the same as when they are present.
- [ ] AC-013 (FR-7): If an issued token is presented on an admin, spend Admin API, or
      telemetry-ingest route, then the system shall reject it as it rejects any non-admin
      credential there.
- [ ] AC-014 (FR-7): If an issued token is presented in `x-api-key`, the `[server.auth]` header,
      or the `jwt_header` / `jwt_cookie` slots, then the system shall not authenticate the request
      with it.
- [ ] AC-015 (FR-8): When a hot reload removes `exchange = true` from the entry that verified an
      issued token, the system shall reject that still-unexpired token with `401` on its next use.
- [ ] AC-016 (FR-8): When a hot reload removes the token's repository from the verifying entry's
      `require` values, the system shall reject that still-unexpired token with `401` on its next use.
- [ ] AC-017 (FR-8): When a hot reload rotates the signing secrets so that the old secret is
      dropped, the system shall reject tokens it signed.
- [ ] AC-018 (FR-8): When a hot reload rotates the signing secrets and keeps the old secret as a
      non-first entry, the system shall keep accepting tokens it signed.
- [ ] AC-019 (FR-5): The system shall not accept an issued exchange token where a gateway-login
      session token is expected.
- [ ] AC-020 (FR-5): The system shall not accept a gateway-login session token as an exchange
      token.
- [ ] AC-021 (FR-11): When an issued token, valid or expired, arrives in `Authorization` on a
      passthrough route, the system shall forward the request upstream without that token.
- [ ] AC-022 (FR-11): When an issued token authenticates a request on a gated route, the system
      shall forward the request upstream without that token.
- [ ] AC-023 (FR-12): If `exchange = true` appears on an entry while `[server.auth.exchange]` is
      absent, then `shunt check` shall fail with an error naming the entry.
- [ ] AC-024 (NFR-4): While `[server.auth.exchange]` is absent, the system shall answer `404` to
      `POST /v1/oauth/token`.

## Success Criteria

- [ ] SC-1: A gh-aw workflow configured as in the issue (`engine.auth.type: github-oidc`,
      `token-url` pointing at shunt) completes a Claude run longer than five minutes through
      shunt, including the threat-detection job, with no static client token.

## Out of Scope

- Rate limiting the exchange endpoint per identity or per source address (to be filed as a
  follow-up issue).
- Pinning an entry to an expected `federation_rule_id` or any other Anthropic federation id.
- Revoking an individual issued token before its expiry. Revocation works per entry (clear its
  opt-in or remove it) or per signing secret (rotate it out).
- Exchanging credentials other than `[[server.auth.jwt]]` JWTs, and any change to the gateway
  login (`[server.gateway]`) token flow.
- Authenticating with an issued token on passthrough routes. These routes are not gated today, so
  the token is stripped there and the caller's own upstream credential applies.
- Other RFC 8693 / RFC 7523 features (refresh tokens, scopes, form-encoded bodies, client
  authentication).

## Assumptions

- The issue's "lesser of a configured maximum and the remaining lifetime the operator allows" is
  read as a single operator knob: `max_ttl_seconds`. The issued lifetime is that value, and it
  never depends on the assertion's expiry. The sidecar refreshes by exchanging a fresh assertion,
  so no other bound is needed.
- The OAuth error shape on this endpoint is a deliberate exception to "gateway-owned errors use
  the Anthropic shape". The AWF sidecar parses OAuth errors here, the way the inbound Codex
  endpoint's clients parse OpenAI errors. AGENTS.md will record the exception.
- All verification failures, including "verified by an entry that has not opted in", map to the
  same `401 invalid_grant` (NFR-3). The issue's `400 invalid_grant` is used only for the cases
  FR-10 lists as `400`.
- A `[server.auth.exchange]` table with no opted-in entry is a configuration mistake, so it is
  rejected rather than warned about (FR-12).
- `max_ttl_seconds` has no upper bound beyond being non-zero. The docs will advise keeping it
  short, because a long lifetime delays per-entry revocation.
