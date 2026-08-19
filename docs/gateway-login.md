# M-A — Claude apps gateway login

## Scope

M-A adds the OAuth 2.0 device-flow surface that lets Claude Code sign in to
shunt with managed `forceLoginMethod: "gateway"` settings. It is opt-in through
`[server.gateway]`; when that table is absent, shunt registers none of the new
login routes and its existing authentication behavior is unchanged.

Implemented endpoints:

| Endpoint | Contract |
| :-- | :-- |
| `GET /.well-known/oauth-authorization-server` | RFC 8414 metadata plus `gateway_protocol_version: 1` |
| `POST /oauth/device_authorization` | RFC 8628 device authorization; 256-bit opaque device code, base-20 `XXXX-XXXX` user code, 600-second lifetime, 5-second polling interval |
| `GET /device` | Browser approval form; a `user_code` query parameter only pre-fills the form and never auto-approves |
| `POST /device` | Same-origin CSRF guard, per-IP attempt limit, optional static-user authentication, and grant approval |
| `POST /device/authorize` | Starts an allowlisted OIDC sign-in only after explicit same-origin browser confirmation, with a single-use state and PKCE |
| `GET /device/callback` | Exchanges the IdP code, verifies the email identity and allowlist, and approves the existing device grant |
| `POST /oauth/token` | Device grant polling and rotating refresh grant |

OAuth failures use the RFC 6749/RFC 8628 `{"error":"..."}` body. For
routes whose selected provider injects a server-side credential, the existing
`/v1/messages` and `/v1/messages/count_tokens` surfaces accept a valid issued
bearer token when gateway mode is enabled; `/v1/models` does as well. Passthrough
providers remain open. Authentication failures keep the Anthropic error envelope.
If `[server.auth]` is also configured, either its static client token or a valid
gateway JWT grants access on those gated routes.

Successful device and refresh grants return the same shape:

```json
{
  "access_token": "<HS256 JWT>",
  "refresh_token": "<opaque rotating token>",
  "token_type": "Bearer",
  "expires_in": 3600
}
```

The JWT contains `sub`, `email`, `name`, `aud: "shunt"`, `iss`, `iat`, and
`exp`. It is signed with HS256 using the secret configured by
`[server.gateway.session] jwt_secret` (or the deprecated `jwt_secret_env`,
which names an environment variable holding it). Refresh tokens are 256-bit
opaque identifiers. Every successful refresh rotates the token; replaying a
used token revokes the active token in that rotation family and returns
`401 {"error":"invalid_grant"}`.

## Configuration

```toml
[server.gateway]
public_url = "https://gateway.example.com"
users_env = "SHUNT_GATEWAY_USERS"           # default
trust_forwarded_for = false                  # default
# state_path = "~/.shunt/gateway-sessions.json"  # default; "" = memory-only sessions

[server.gateway.session]
jwt_secret = "${SHUNT_GATEWAY_JWT_SECRET}"
ttl_hours = 1                                # default
```

```bash
export SHUNT_GATEWAY_JWT_SECRET="$(openssl rand -base64 48)"
export SHUNT_GATEWAY_USERS='alice@example.com:<secret>,bob@example.com:<secret>'
```

`jwt_secret` is required when `[server.gateway.session]` is present, needs at
least 32 bytes of entropy, and signs the HS256 access JWTs. Being a
`Secret`-typed field, it accepts `${VAR}` / `${file:/abs/path}` references and
is redacted in any config echo. It also accepts an array for rotation — index
0 signs new tokens and every entry verifies:

```toml
[server.gateway.session]
jwt_secret = ["new-secret-value", "old-secret-value"]
```

Rotate by prepending the new secret, waiting `ttl_hours` for outstanding
access tokens to expire, then dropping the old entry. `ttl_hours` sets the
access-token lifetime in whole hours and defaults to `1`; raise it (e.g. `8`
or `12`) when the IdP issues no refresh token (no `offline_access`), since
there is then no silent refresh and a short TTL just sends developers back to
the browser login more often.

`jwt_secret_env` (default `SHUNT_GATEWAY_JWT_SECRET`, an environment-variable
name) and `token_ttl_seconds` (default `3600`) are deprecated equivalents,
still fully supported with no behavior change when used alone —
`token_ttl_seconds` remains the only way to express a sub-hour lifetime, since
`ttl_hours` is whole hours only. Setting both a deprecated key and its
`session.*` replacement fails startup, evaluated per key: `jwt_secret_env`
together with `session.jwt_secret` is an error, and `token_ttl_seconds`
together with `session.ttl_hours` is an error; mixing across the two pairs
(e.g. `token_ttl_seconds` alongside `session.jwt_secret`) is fine. shunt logs
one deprecation warning whenever a deprecated key is explicitly set —
whether in the config file or through a `SHUNT_*` environment override —
and stays silent only when the key itself is never configured; a config
that never sets `jwt_secret_env` and simply relies on the
`SHUNT_GATEWAY_JWT_SECRET` env var holding the secret still doesn't warn,
since that variable holds the secret's value, not the deprecated key being
set. Where only one side of a pair is set, `session.*` wins if present, otherwise the deprecated key, otherwise the
default.

Startup fails closed if `public_url` is not a bare HTTPS origin (`http` is
accepted only on loopback), the token TTL is zero, the JWT secret is shorter than
32 bytes, or neither a valid static-user list nor an external IdP is configured.
Secret, user, and IdP changes are re-resolved by config hot reload. Whether the
routes exist is fixed at boot, so adding or removing `[server.gateway]` requires a
restart.

An external IdP replaces the password form when `users_env` is unset or empty:

```toml
[server.gateway.oidc]
issuer = "https://accounts.google.com"
client_id = "gateway-client-id"
client_secret_env = "SHUNT_GATEWAY_OIDC_SECRET" # default
allowed_domains = ["example.com"]
# allowed_emails = ["contractor@outside.example"]
```

`[server.gateway.oidc]` exposes the following keys:

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `issuer` | required | OIDC discovery issuer; HTTPS is required except for loopback HTTP |
| `client_id` | required | Non-empty OIDC client identifier |
| `client_secret_env` | `SHUNT_GATEWAY_OIDC_SECRET` | Environment variable holding the non-empty client secret |
| `allowed_domains` | `[]` | Case-insensitive email domains allowed to approve a device |
| `allowed_emails` | `[]` | Case-insensitive full email addresses allowed to approve a device |
| `scopes` | `openid email profile` | Scopes sent to the authorization endpoint; custom values must include `openid` and `email` |
| `authorization_endpoint` | OIDC discovery | Advanced authorization URL override |
| `token_endpoint` | OIDC discovery | Advanced token URL override |
| `userinfo_endpoint` | OIDC discovery | Advanced UserInfo URL override |

At least one `allowed_domains` or `allowed_emails` entry is required. When an
endpoint override is omitted, shunt resolves it from
`{issuer}/.well-known/openid-configuration` and requires the returned issuer to
match the configured issuer exactly. Every configured or discovered endpoint
must use HTTPS, except that HTTP is accepted on loopback. Providers that do not
expose standard OIDC, including GitHub or a SAML identity provider, should be
connected through an OIDC broker such as Dex; direct OAuth2 integrations are
out of scope.

## Pluggable approval

The HTTP endpoints depend on the `ApprovalProvider` trait rather than on the
static-user implementation directly. `GatewayAuth::with_approval_provider`
accepts an `Arc<dyn ApprovalProvider>` with its existing constructor shape, while
`GatewayAuth::with_optional_approval(...).with_oidc(...)` builds an external-IdP
or mixed approval surface. `StaticUsers` resolves comma-separated
`email:secret` entries from `users_env`, compares secrets in constant time, and
emits an identity with `sub = email`, `email = email`, and `name` set to the local
part before `@`.

The shipped external provider supports discovery-based OIDC. It accepts only a
non-empty UserInfo email with `email_verified = true`, then applies the
case-insensitive domain/email allowlist. The browser leg uses a 256-bit,
single-use, ten-minute state plus PKCE, and token/UserInfo calls have a ten-second
timeout. The gateway deliberately uses the authenticated UserInfo endpoint rather
than validating an ID token against JWKS: it needs the current verified email
claim for authorization, while the access token remains server-side. The static
form remains available when `users_env` is populated; when it is absent, only the
SSO button is rendered.

The browser form is server-rendered and uses no client-side script. Its mutation
is accepted only with a same-origin `Origin` or `Referer`, a same-origin/same-site
Fetch Metadata signal, or a browser-navigation `Sec-Fetch-Site: none` request
without contradictory cross-site hints. A rejected request returns a human-readable
HTML error page with a non-success HTTP status.

## State and operational boundary

Device grants, IdP states, discovery results, and rate-limit counters are
process-lifetime, in-memory stores that survive a config hot reload. Refresh-token
sessions also survive hot reload and are persisted by default as described below.
IdP states and device grants expire after ten minutes and each rejects new
admission at 4,096 live entries. Mutating operations remove expired device
grants, IdP states, and idle rate-limit entries. Used refresh-token tombstones are retained
for 30 days and capped at 64 per family, preserving bounded replay detection
without process-lifetime growth; active refresh tokens that go 30 days without
rotating expire the same way.

The refresh-token store additionally persists to `state_path` by default
(`~/.shunt/gateway-sessions.json` — the directory shunt's account stores
already use; issue #194), mirroring the pool quota cache
(`src/state_persist.rs`): the token endpoint writes the store — atomically,
owner-only permissions (0600 on Unix) — after every grant, rotation, or replay
revocation, before the response is sent, and boot restores it before serving,
so a restart keeps managed logins alive. Tokens are keyed by SHA-256 both in
memory and on disk (they are 256-bit random, so an unsalted hash suffices), so
the file never holds a usable credential — only token hashes, rotation-family
ids, timestamps, and the signed-in identities. Reading is best-effort: a
missing, corrupt, or version-mismatched file falls back to memory-only
behavior, never a boot failure. Setting `state_path = ""` keeps sessions
memory-only — then restarting shunt invalidates outstanding refresh tokens,
and existing access JWTs remain valid until expiry, after which users must
sign in again; an environment with no resolvable home directory behaves the
same. Device grants and rate-limit counters stay memory-only by design (a
restart mid-login only costs that attempt). The state file is single-process;
sharing grants and replay detection between concurrent gateway instances
remains a follow-up, and this change deliberately adds no database.

Refresh grants mint tokens from the identity stored with the session and do
not re-check `users_env` or the external IdP allowlist, so removing a user from
either approval source does not end an existing session — with persistence
default-on, the session survives up to the 30-day idle horizon. To deprovision a
user immediately, also delete the state file (or set `state_path = ""`) and
restart.

Use TLS for a non-loopback deployment. By default `/device` rate limiting uses
the socket peer and ignores `X-Forwarded-For` and `X-Real-IP`. Set
`trust_forwarded_for = true` only when every request reaches shunt through a
trusted reverse proxy that strips client-supplied forwarding headers and adds
its own trusted client address. Enabling it on a directly exposed gateway lets
clients choose their rate-limit identity.

A gateway login session also has the reference gateway's reduced Claude Code
feature set, and the restrictions are client-side — shunt cannot lift them by
supporting a feature upstream. Per Anthropic's
[Claude apps gateway](https://code.claude.com/docs/en/claude-apps-gateway)
reference, on a signed-in gateway session WebSearch is disabled, the
extended-cache-ttl beta and first-party-only optimizations (global cache scope,
token-efficient tools) are omitted, the gateway token is the session's only
credential, startup is fail-closed after about 10 seconds when the gateway is
unreachable, and telemetry is OTLP over HTTP only. There is no service-token
flow for CI. Artifact publishing is unavailable on a gateway token
([artifacts](https://code.claude.com/docs/en/artifacts)); usage analytics,
error reporting, and survey ratings to Anthropic are disabled with no setting to
re-enable them ([data usage](https://code.claude.com/docs/en/data-usage)); and
server-managed settings do not reach gateway-routed sessions
([rollout](https://code.claude.com/docs/en/llm-gateway-rollout)), which is what
`GET /managed/settings` below replaces. Auto mode no longer needs
`CLAUDE_CODE_ENABLE_AUTO_MODE` — that variable is now accepted for compatibility
and has no effect — but on gateway sessions it is restricted to Claude Sonnet 5,
Opus 4.7 or later, and Fable 5
([permission modes](https://code.claude.com/docs/en/permission-modes)).

Sign-in requires a browser. Personal single-user installations that do not need
managed identity should continue to use `ANTHROPIC_BASE_URL` and, when needed,
`[server.auth]`; that path trips a much smaller set of client restrictions.

A client-side CLI route trades that feature set differently. `shunt gateway
login <url>` runs the same device flow from the terminal and stores the issued
session locally; `shunt gateway token` prints the access token for Claude Code's
`apiKeyHelper`, and `shunt gateway claude` launches Claude Code with that wiring
applied to a single process. **This does not change the browser requirement
above** — approval still happens on the `/device` page, `POST /device` is still
same-origin protected, and an OIDC-only deployment still renders no password
form. What it changes is which credential slot the token arrives in on the
client, and that slot is what selects Claude Code's provider mode: a
helper-supplied credential leaves the session in the client's ordinary
first-party mode, so the restrictions tabulated above — which apply to a
signed-in gateway session — are not taken on, and the `opus`/`sonnet` aliases
are not remapped to the older ids a gateway session pins (measured against
Claude Code 2.1.234). One tier does not survive, and for a reason unrelated to
the gateway session: the client gates Fable availability on an exact host match
against `api.anthropic.com`, so pointing `ANTHROPIC_BASE_URL` at any other
host — shunt included — hides Fable from the model picker and from the
availability check automatic tier selection consults. `ANTHROPIC_DEFAULT_FABLE_MODEL`
overrides that gate, being read ahead of the host check rather than acting only
as an alias-to-id mapping, so a session that needs Fable can set it explicitly;
`shunt gateway claude` does not set it for you. That is narrower than "no restrictions": supplying any
credential, `apiKeyHelper` included, trips Claude Code's separate credential-type
gate (prompt-cache TTL defaults, Remote Control, voice dictation, artifact
publishing), which is independent of the signed-in session. The cost is symmetrical: a helper-credential session was
not observed fetching `GET /managed/settings` (same measurement), so per-user
policy enforcement in the client remains a `forceLoginMethod: "gateway"`
property. Users are still individually identified at the gateway either way,
because the token is the same per-user device-flow session.

The launcher does not rely on the settings document alone to reach the helper.
Claude Code applies a settings `env` block over the inherited environment, so
every ambient variable the block does not name survives into the child: an
exported `ANTHROPIC_AUTH_TOKEN` would beat `apiKeyHelper` outright (the helper
is consulted only when that variable is absent), and `CLAUDE_CODE_USE_GATEWAY`
would flip the session into gateway provider mode, a path that never consults
the helper at all. `shunt gateway claude` therefore removes 34 credential and
provider-mode variables from the launched process — the Anthropic/AWS/Foundry
credential variables, the `CLAUDE_CODE_USE_*` provider selectors and their
region/project siblings, the `CLAUDE_CODE_SKIP_*_AUTH` modifiers, the
host-managed indirection variables, `ANTHROPIC_CUSTOM_HEADERS`, and both
`*_FILE_DESCRIPTOR` readers — plus, resolved at launch, whatever variable
`CLAUDE_CODE_HOST_AUTH_ENV_VAR` names, since that one points at an arbitrary
name no fixed list can express. `ANTHROPIC_BASE_URL` is deliberately left in
place; the settings document injects it. This closes the **ambient-environment**
channel and only that: a settings-file `env` block is re-applied after launch, an
`apiKeyHelper` may already be set in the user's own settings, an existing saved
login lives in the credential store, and both file-descriptor readers fall back
to a well-known path consulted with no variable set at all. It is not a
guarantee that the session runs in first-party mode.

The transport floor on that client route has one narrow widening. A plain-http
gateway — `shunt gateway login http://10.0.0.5:8080`, accepted with the
unencrypted-traffic warning that also repeats on every refresh — may advertise
plaintext endpoints on its **own origin** (scheme, host, and port all matching
the operator-supplied base URL), and the login completes. A plaintext endpoint
on any other origin is refused, as is any plaintext endpoint at all when the
gateway itself is reached over https. The allowance is computed from the
operator-supplied base URL and never from the discovery document, so a hostile
or MITM'd document cannot name a third-party plaintext host and have the
refresh token POSTed there.

For per-user policy after sign-in, shunt now serves authenticated
`GET /managed/settings` with ordered email matching, `ETag`/`304`, telemetry
environment push, and `availableModels` enforcement. See the
[M-B managed-settings note](gateway-managed-settings.md).

## Follow-ups

- **M-C:** authenticated inbound OTLP `POST /v1/{metrics,logs,traces}` sink and
  optional verbatim relay.
- **Multi-instance session sharing:** move refresh sessions (and device grants)
  behind a shared backend (e.g. PostgreSQL) that owns the state, with atomic
  token rotation, so concurrent gateway instances agree on grants and replay
  detection. The store's narrow `issue`/`rotate`/`export`/`import` surface and
  epoch-based, hash-keyed records are designed to make that a contained
  swap; the `state_path` file stays the single-process default.
