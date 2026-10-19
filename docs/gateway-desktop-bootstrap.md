# Claude Desktop bootstrap endpoint

Issue: [#787](https://github.com/pleaseai/shunt/issues/787). Builds on the
[M-A device-flow login](gateway-login.md) and the
[M-B policies](gateway-managed-settings.md).

## Scope

Claude Desktop's third-party inference can fetch its whole configuration from a
*bootstrap URL* and sign the user in with an OAuth device-code grant against the
same server (Anthropic's
[bootstrap documentation](https://claude.com/docs/third-party/claude-desktop/bootstrap),
mode "Bootstrap server as authorization server (device code)"). shunt serves
that contract: one `GET /user/bootstrap` route that returns an operator-supplied
JSON template, plus a small change to the existing device-code login so the
client Desktop actually sends is accepted.

This replaces the earlier Desktop setup that used
`inferenceCredentialKind: "helper-script"` with an absolute `shunt` path. That
path differs between Homebrew on Apple Silicon (`/opt/homebrew/bin`) and Intel
(`/usr/local/bin`), and it needs the CLI installed and logged in on every device.

The route is opt-in twice over: it exists only when `[server.gateway]` is set,
and it answers `404` until `[server.gateway.desktop_bootstrap]` names a template.

## Configuration

```toml
[server.gateway.desktop_bootstrap]
template = "/etc/shunt/claude-desktop-bootstrap.json"   # file path only
```

- The template is a **file path**. A URL source is not supported (see
  [Follow-ups](#follow-ups)).
- At config load and `shunt check` the file must exist and parse as a JSON
  object; otherwise config load fails.
- The file is read on every request, so edits apply without a restart.
- A relative path resolves against shunt's working directory, not the
  directory of the config file.

## `GET /user/bootstrap`

| Status | Body | When |
| :-- | :-- | :-- |
| `200` | the template, `application/json` | signed-in, entitled user |
| `304` | empty | `If-None-Match` matches the current `ETag` |
| `401` | `authentication_error` | bearer missing, invalid, or expired |
| `403` | `permission_error` | valid bearer, user not entitled |
| `404` | `not_found_error` | `desktop_bootstrap` not configured |
| `500` | `api_error` | the template became unreadable or is no longer a JSON object at runtime |

The bearer is the gateway access token issued by the existing device-code login;
static `[server.auth]` tokens do not authenticate this route. Every response,
including errors and `304`, carries `Cache-Control: no-store`. The route never
redirects, because Desktop treats a `3xx` as an error.

### Entitlement

Entitlement reuses `[[server.gateway.policies]]`:

- with no policies configured, every signed-in user is entitled;
- with policies, a user is entitled when a policy's `match.emails` lists them
  (compared case-insensitively), or when any catch-all policy (no `match`)
  exists.

Only entitlement is evaluated. The policies' `cli` settings are not merged into
the bootstrap response.

### Body and placeholders

The template is passed through **unchanged**, with no reshaping. Claude Desktop
accepts both the nested `bootstrap-config-v2` format (what its own JSON export
writes) and the flat v1 format, so shunt does not need to know which one the
operator chose.

The only edit is inside string values: `${user.email}`, `${user.sub}`, and
`${user.name}` are replaced with the signed-in user's values. Unknown `${...}`
tokens are left as-is, and object keys are never substituted.

Nested v2 example:

```json
{
  "$schemaVersion": 2,
  "inference": {
    "provider": "gateway",
    "baseUrl": "https://shunt.example.corp",
    "credential": { "kind": "interactive" }
  },
  "otlp": {
    "resourceAttributes": { "user.email": "${user.email}" }
  }
}
```

Flat v1 equivalent:

```json
{
  "inferenceProvider": "gateway",
  "inferenceGatewayBaseUrl": "https://shunt.example.corp",
  "inferenceCredentialKind": "interactive",
  "otlpResourceAttributes": { "user.email": "${user.email}" }
}
```

Response keys are effective configuration: a key the template omits is unset on
the device, not inherited from MDM. Return every key you want applied.

### Caching

The `ETag` is a quoted `"sha256:…"` over the response body. A matching
`If-None-Match` returns `304`, so Desktop's background re-check (every 10 minutes
by default) costs a `304`.

## OAuth change: `client_id` is optional

Claude Desktop 2.42304.2 sends no `client_id` on the device authorization
request, the token poll, or the refresh grant. The login endpoints now accept a
request without one:

| Request | Result |
| :-- | :-- |
| no `client_id` | accepted |
| `client_id=claude-code` | accepted, behaviour unchanged |
| any other `client_id` | still rejected |

A rejected refresh token also differs by client. For a request **without**
`client_id`, shunt returns `400 {"error":"invalid_grant"}` (RFC 6749 §5.2):
Desktop treats only `400` plus `invalid_grant` as a revoked session and anything
else as transient. `claude-code` requests keep the existing
`401 {"error":"invalid_grant"}` described in the [M-A note](gateway-login.md).

## How Desktop uses it

- Set only `bootstrapUrl` to `<public_url>/user/bootstrap`. Desktop strips a
  trailing `/bootstrap` or `/user/bootstrap` to find the RFC 8414 issuer base, so
  shunt's existing `/.well-known/oauth-authorization-server` is discovered. It
  then runs the device-code grant against shunt, and the user approves on shunt's
  `/device` page (password or the configured OIDC IdP).
- The origin of `bootstrapUrl` must equal the origin of
  `[server.gateway] public_url`: every discovery and verification URL must share
  the bootstrap origin.
- **Origin pinning.** Without `bootstrapOidc`, Desktop drops
  `inferenceGatewayBaseUrl` unless it shares the `bootstrapUrl` origin, so the
  template's gateway base URL must be shunt's `public_url`.
- **One sign-in.** When the gateway base URL shares the bootstrap origin and the
  credential kind is `interactive`, Desktop reuses the bootstrap bearer for
  inference and, from 1.34493.0, renews it with `grant_type=refresh_token`.
- Desktop holds the response in memory only. If the gateway is unreachable at
  launch it starts in a degraded sign-in state, the same dependency inference
  already has.

### HTTPS and loopback

Anthropic's documentation describes the bootstrap endpoint as HTTPS, and the
packaged Desktop app drops loopback URLs (`127.0.0.1`, `localhost`, `[::1]`) from
response keys. A deployment needs a real hostname with TLS in front of shunt; a
bare `127.0.0.1` deployment does not work for this mode.

## Device-side seed file

Users import this file through **Developer → Configure Third-Party
Inference… → Import configuration**:

```json
{
  "bootstrapUrl": "https://shunt.example.corp/user/bootstrap",
  "trustBootstrapDelivery": true
}
```

`trustBootstrapDelivery` skips the "Apply settings from your organization?"
consent dialog that otherwise appears for base-URL, helper, and marketplace
values a bootstrap response delivers to a device whose `bootstrapUrl` came from a
local file. Desktop accepts it only from MDM or the local config file, never from
the bootstrap response.

## Follow-ups

- **Template URL source.** `template` is a file path only; fetching a template
  from a URL is not supported.
- **Revocation.** shunt does not advertise a revocation endpoint, so signing out
  of Desktop is local-only and does not invalidate the refresh session on the
  gateway.
