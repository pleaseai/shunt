# Inbound JWT authentication (verify-only)

Issue [#344](https://github.com/pleaseai/shunt/issues/344), phase 1.

## 0. Problem

A shared shunt deployment has two inbound options, and neither fits an organization that
already runs an identity provider:

- **`[server.auth]` static token** — no per-user identity, no revocation, one shared secret
  handed to everyone.
- **`[server.gateway]` device login** — per-user identity, but signing in through the
  gateway puts Claude Code into gateway provider mode, which disables a set of client
  features (enumerated in `gateway-login.md` §"State and operational boundary"), and shunt
  becomes an auth server with sessions, refresh tokens, and a signing secret to own.

`[[server.auth.jwt]]` is the third option: per-user identity **without** the client-side
feature loss. The client is configured exactly as it is for a static token — a base URL plus
a bearer — so it stays in first-party mode; the bearer is simply a JWT shunt can verify
instead of a string it compares.

shunt issues nothing here. No login flow, no session store, no refresh token, no signing
secret. Token lifetime and revocation belong to the issuer, so shunt stays stateless across
replicas and never acquires the deprovisioning problem the gateway path has.

## 1. Configuration

`[[server.auth.jwt]]` is an **array** of tables under `[server.auth]`. An array from the
start because one issuer with several audiences is a normal configuration (separate client
ids for humans and for services), and because human SSO and workload identity use different
claim vocabularies.

```toml
[server.auth]
# Optional once a JWT issuer is configured: a deployment that authenticates
# entirely through an IdP has no static tokens to set.
tokens_env = "SHUNT_CLIENT_TOKENS"

[[server.auth.jwt]]
issuer = "https://accounts.google.com"
audience = ["<client id dedicated to shunt>"]
email_domains = ["example.com"]
```

| Key | Required | Default | Meaning |
| :-- | :-- | :-- | :-- |
| `issuer` | yes | — | Exact `iss` match. HTTPS, except HTTP on loopback; a path is allowed |
| `audience` | yes | — | Accepted `aud` values; a string or an array, so a client-id rotation is not a flag day |
| `email_domains` | — | `[]` | Case-insensitive **exact** match on the part after the final `@` — never a suffix match |
| `allowed_emails` | — | `[]` | Case-insensitive full-address match |
| `algorithms` | — | `["RS256"]` | Accepted signing algorithms. Asymmetric only |
| `authorized_parties` | — | `audience` | Accepted `azp` values when the claim is present |
| `clock_skew_seconds` | — | `0` | Tolerance on `exp` and `nbf` |
| `max_token_age_seconds` | — | `3600` | Reject when `exp - iat` exceeds this |
| `jwks_url` | — | discovery | Explicit JWKS endpoint, for issuers that serve no discovery document |

Validation, all at startup:

1. `tokens_env` may resolve empty **only** when at least one entry exists. With neither a
   resolvable token list nor an entry, `[server.auth]` still fails closed.
2. Each entry needs `issuer`, `audience`, and at least one `email_domains` /
   `allowed_emails` value.
3. `algorithms` must be non-empty and must not name a symmetric algorithm (`HS256`, `HS384`,
   `HS512`).
4. An entry's `issuer` may not equal `[server.gateway] public_url`.

Rule 2 is enforced rather than documented because for some issuers `audience` is not an
authorization decision at all: a GitHub Actions workflow chooses its own `aud`, so any
repository on GitHub can mint a token carrying a given audience. Phase 2 adds the operators
(`require`, `subject_prefix`) that authorize such issuers; phase 1 offers only the email
ones, which is why an email rule is currently mandatory.

Rule 4 keeps the two JWT paths unambiguous. `[server.gateway]` mints session tokens whose
`iss` is its own `public_url`, and both verifiers read the same `Authorization: Bearer`
slot, so a shared issuer would make ownership of a token depend on evaluation order.

## 2. Verification

Order matters, and each rule closes a specific hole.

**Entry selection.** Collect every configured entry whose `issuer` equals the token's
*unverified* `iss` and accept if any one of them fully validates — not the first match,
since duplicate issuers are normal. Routing on an unverified claim is safe because the
selected entry's key set is then authoritative: claiming another issuer only picks a JWKS
the token cannot satisfy, and `iss` is re-checked against the verified payload before
anything is accepted.

**Algorithm.** Pinned from config; the token header's `alg` never selects it. Symmetric
algorithms are refused at config time, which is what stops a published JWKS key from being
replayed as an HMAC secret.

**Key.** `kid` is required and looked up in the cached JWKS — shunt never tries every key.

**Claims.** `exp` and `nbf` with `clock_skew_seconds`; `aud` against the configured set;
`azp` against `authorized_parties` when the claim is present; `exp - iat` against
`max_token_age_seconds`; `email_verified` must be `true`; the address must match an email
rule.

**Identity.** The verified `email` becomes the caller identity, in place of the `name` from
a `name:token` pair. It is capped at 256 bytes: the identity namespaces the account pool's
sticky key on the inbound Codex endpoint, so an unbounded caller-controlled string there
would be the failure mode issue #296 records for the Codex `model` label.

### Key sets

Fetched **lazily**, on first use per issuer, so a briefly unreachable IdP does not block
boot. Discovery (`{issuer}/.well-known/openid-configuration` → `jwks_uri`) is used unless
`jwks_url` overrides it, and the discovery document's own `issuer` must match the configured
one. Every endpoint — discovered or configured — must be HTTPS, except HTTP on loopback,
and both requests share the 10-second budget `gateway/idp_client.rs` uses. A response over
256 KiB is refused rather than truncated: a truncated JWKS would parse as "this issuer has
fewer keys than it does" and silently reject tokens signed with the ones cut off.

An unknown `kid` triggers at most one refetch per **60-second** window per issuer, so forged
`kid` values cannot be used to make shunt hammer the issuer. A failed fetch is rate-limited
the same way. When a fetch fails but a previously-fetched key set is cached, that cache is
still the best available answer and an unknown `kid` remains a `401`.

The cache is per-issuer, and so is the failure domain: one issuer's outage must not deny the
others. It lives on `AppState` (beside `admin_stores` and `gateway_stores`) rather than on
the hot-reloaded `InboundAuth`, so a config reload re-resolves the entries without
discarding keys — otherwise a reload would refetch every configured issuer, and could be
repeated to make shunt do so.

### Status codes

- **`401`** — no credential, or one that verified against no entry. Every failure reason
  collapses into one response; nothing discloses which check failed.
- **`503`** — a matching entry's key set could not be fetched, so no verdict was possible.
  Reported only when *no* entry reached a verdict: a token a reachable entry rejected is a
  `401` even if another entry for the same issuer happened to be unreachable.

The distinction matters operationally. A `401` for an IdP outage sends an operator hunting a
credential that is fine.

## 3. Credential precedence

With `[server.auth]` tokens, `[server.gateway]`, and JWT entries all configured, a gated
route accepts three credentials. On the `Authorization: Bearer` slot: static token first (a
constant-time compare, no network), then the gateway JWT on the routes that accept it, then
JWT entries by `iss`. First success wins.

A JWT is accepted **only** in `Authorization: Bearer` — not in the configured
`[server.auth]` header and not in `x-api-key`. That is the slot Claude Code sends
`ANTHROPIC_AUTH_TOKEN` in, which is the point: the client needs no shunt-specific
configuration.

**A verified JWT is never forwarded upstream.** `m4-inbound-auth.md` §2 states the boundary
for static tokens and resolves the mixed passthrough/mapped case by advice: hand out
dedicated `x-shunt-token` values so the bearer slot stays free to carry each caller's real
upstream credential. That advice does not exist for a JWT, which is only ever accepted in
the bearer slot — so the strip has to be explicit. Two places consume the bearer and had to
learn about it: the passthrough attempt of a gated route chain (`proxy/failover.rs`), and
the passthrough credential relay in upstream model discovery
(`discovery/upstream.rs`). Without both, shunt would relay an identity token from the
operator's IdP to a third-party upstream.

The gate covers the routes `[server.auth]` already guards — injected-credential
`/v1/messages` and `/v1/messages/count_tokens`, `GET /v1/models`, `GET /usage`,
`GET /api/oauth/usage`, and the inbound Codex Responses and analytics routes. Passthrough
inference is never checked, because the caller pays with their own credential; "per-user identity" therefore
covers the gated routes, not the whole surface.

## 4. Accepted tradeoffs

**No replay protection beyond expiry.** A JWT is a bearer token: whoever holds it can use it
until `exp`. shunt deliberately keeps no `jti` denylist, because that would reintroduce the
shared state this design exists to remove. `max_token_age_seconds` is what bounds the
exposure, which is why it is part of the core design rather than a nicety — deployments
should issue minutes-scale tokens.

**Verification is only as good as the issuer's own lifecycle.** shunt re-checks the
allowlist on every request, so removing a domain or address takes effect immediately. But a
token already minted for a still-allowed address keeps working until it expires, whatever
the IdP does to the account behind it.

## 5. Not in phase 1

- `require` (claim → allowed values) and `subject_prefix`, the operators workload issuers
  need.
- `identity_claim`, and therefore any issuer whose tokens carry no `email`.
- Per-entry `email_verified`; it is currently unconditional, which is correct while email is
  the only authorization operator.
- Groups. See issue #347.

## 6. Not in scope at all

- Issuance of any kind.
- Non-OIDC providers. GitHub user login is plain OAuth2 with no ID token; it connects
  through an OIDC broker such as Dex, as `gateway-login.md` already states.
- Extending `[server.gateway] policies` (managed settings) to these identities. Managed
  settings only reach gateway-mode sessions.
- Changes to `[server.gateway]`. This is additive; the device-login path is untouched.
