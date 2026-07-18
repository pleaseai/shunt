# M-B — Per-user managed settings

## Scope

M-B extends the opt-in `[server.gateway]` surface with authenticated per-user
managed settings. It adds `GET /managed/settings`, ordered email policy matching,
conditional requests with `ETag`, the managed telemetry environment push, and
server-side enforcement of `availableModels` on Messages requests.

The existing gateway-login routes remain unchanged. When `[server.gateway]` is
absent, none of the gateway routes exist. When gateway login is enabled but
`policies` is absent, `GET /managed/settings` returns the explicit “no managed
policy” response.

## Wire contract

| Request | Response |
| :-- | :-- |
| Valid gateway bearer, matching or catch-all policy | `200` with the resolved document |
| Valid gateway bearer, policies configured but no user-specific match | `200` with the catch-all result; without a catch-all, a telemetry-only `env` when telemetry is enabled, otherwise `{}` |
| Valid gateway bearer, no `policies` configured | `404 not_found_error` |
| Missing, expired, or invalid gateway bearer | `401 authentication_error` |
| Matching `If-None-Match` | `304`, the same `ETag`, and an empty body |

A successful response has this shape:

```json
{
  "uuid": "sha256:<stable-per-subject digest>",
  "checksum": "sha256:<settings digest>",
  "settings": {
    "availableModels": ["claude-opus-4-8"],
    "env": { "DISABLE_UPDATES": "1" }
  }
}
```

`checksum` is the SHA-256 digest of the serialized `settings` JSON bytes. The
response `ETag` is that checksum as an RFC-quoted entity tag, for example
`"sha256:…"`. `If-None-Match` accepts comma-separated candidates, quoted tags,
weak tags such as `W/"sha256:…"`, `*`, and the legacy unquoted checksum form.
`uuid` is stable for the JWT subject and does not expose the subject itself.

`404` and an empty policy are intentionally different:

- No `policies` key means the operator has not configured managed policy, so the
  endpoint returns `404`.
- Configured policies with no matching user or catch-all settings still return
  `200`. The response contains a telemetry-only `settings.env` when telemetry is
  enabled, and `settings: {}` otherwise.

## Configuration

```toml
[server.gateway]
public_url = "https://gateway.example.com"

[[server.gateway.policies]]
[server.gateway.policies.match]
emails = ["alice@example.com"]
[server.gateway.policies.cli]
availableModels = ["claude-opus-4-8"]
[server.gateway.policies.cli.env]
DISABLE_UPDATES = "1"

# Omit match, use `match = {}`, or omit emails for a catch-all policy.
[[server.gateway.policies]]
match = {}
[server.gateway.policies.cli.permissions]
deny = ["WebFetch"]

[server.gateway.telemetry]
[[server.gateway.telemetry.forward_to]]
url = "https://collector.example.com"
headers = { "x-api-key" = "collector-secret" }
```

`cli` is an open-schema `managed-settings.json` object. shunt preserves unknown
keys instead of pinning the gateway to one Claude Code settings version, but all
values must be JSON-representable; non-finite floats are rejected. A configured
empty policy list, scalar/non-object `cli`, empty `emails` list, blank email,
malformed `availableModels` or `env`, or non-HTTP(S) telemetry destination fails
startup.

Policy and telemetry edits hot-apply through the existing gateway auth snapshot.
Adding or removing `[server.gateway]` itself still requires a restart because
route registration is fixed at boot.

## Matching and merge rules

Policies are ordered:

1. Every catch-all policy is merged in list order to form the base.
2. The first non-catch-all policy whose `emails` contains the authenticated JWT
   email is merged on top. Email matching is exact and case-sensitive.
3. Later matching user policies are ignored.

A missing `match`, `match = {}`, or a match without `emails` is catch-all. An
explicit empty `emails = []` is rejected to avoid an ambiguous policy.

Objects merge recursively. Scalars and unlike types are replaced by the overlay.
Arrays normally replace the base array, including allow-lists such as
`availableModels` and `permissions.allow`. When the array's key contains `deny`
case-insensitively, arrays are unioned instead: base order is retained and new,
non-duplicate overlay values are appended. This makes deny policy cumulative
without accidentally broadening an allow-list.

## Telemetry environment push

A non-empty `[server.gateway.telemetry].forward_to` list injects these six string
values into the resolved `settings.env`:

```text
CLAUDE_CODE_ENABLE_TELEMETRY=1
OTEL_METRICS_EXPORTER=otlp
OTEL_LOGS_EXPORTER=otlp
OTEL_TRACES_EXPORTER=otlp
OTEL_EXPORTER_OTLP_ENDPOINT=<server.gateway.public_url>
OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
```

The public URL has no trailing slash or path because gateway validation requires
a bare origin. Injection is a base layer: a policy-provided `env` value wins on
the same key, and unrelated policy env keys are retained.

This M-B table only gates the managed environment push. The destination headers
are accepted now for forward compatibility, but authenticated OTLP ingest and
relay routes `POST /v1/{metrics,logs,traces}` land in M-C (#189).

## `availableModels` enforcement

For a request authenticated with a gateway JWT, shunt resolves the same per-user
policy before forwarding `/v1/messages` or `/v1/messages/count_tokens`. Before
comparison, shunt strips one trailing Claude Code context-window hint (`[1m]` or
`[1M]`) from the client-requested top-level `model`; the remaining model must be
present in a resolved `availableModels` array of strings. For example,
`allowed[1m]` is permitted by `availableModels = ["allowed"]`. Otherwise, shunt
returns:

```json
{
  "type": "error",
  "error": {
    "type": "invalid_request_error",
    "message": "model \"claude-sonnet-4-6\" is not permitted by this gateway's managed policy"
  }
}
```

The status is `400`, and no upstream request is made. No `availableModels` key,
no managed policy, or authentication through the separate static
`[server.auth]` client-token path leaves model selection unrestricted.
