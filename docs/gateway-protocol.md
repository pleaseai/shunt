# Claude Code gateway protocol — reference

> The complete wire contract between the Claude Code CLI and an LLM gateway: sign-in,
> inference, managed settings, model discovery, and telemetry. This is the organized
> companion to the verbatim capture in [`gateway-protocol.raw.md`](gateway-protocol.raw.md).
>
> **Two upstream sources, and how they relate:**
> - Public docs: [LLM Gateway Protocol](https://code.claude.com/docs/en/llm-gateway-protocol)
>   — the header/body **pass-through** contract (what to forward, what breaks when stripped).
> - Machine-readable `GET /protocol`, served by a running Claude apps gateway — a **superset**
>   that also specifies the OAuth device-flow sign-in, managed-settings, and telemetry
>   endpoints. `gateway-protocol.raw.md` is a byte-for-byte capture of that endpoint.

## Provenance of the raw capture

| Field | Value |
| :-- | :-- |
| Endpoint | `GET /protocol` (unauthenticated, `text/markdown`) |
| Served by | `claude gateway` (the Claude apps gateway, built into the `claude` binary) |
| Binary | Claude Code `2.1.207`, native Mach-O x86_64 |
| Captured | 2026-07-13, from a local minimal gateway (Postgres + Google OIDC discovery + Bedrock upstream) |
| Body | 9709 bytes, `sha256 ee13692271dc1a1382b6c063d66bd9fde7fa3f2e4e8c0a8e4940bbd49b1c5f88` |
| Raw file | [`gateway-protocol.raw.md`](gateway-protocol.raw.md) |

The gateway serves this contract from itself so it always matches the running version. It is a
**more stable target than proxying raw CLI traffic**: auth is standard OAuth 2.0, inference is
the Messages API, headers are the lowest common denominator across backends. Backwards
compatibility is kept "within reason," not forever — managed settings is called out as the
endpoint most likely to change.

## Sign-in / request flow

1. Client fetches `GET {base}/.well-known/oauth-authorization-server`.
2. On first contact, client fingerprints the gateway's TLS leaf certificate and asks the user
   to trust it (pinned per-hostname; re-prompts on mismatch).
3. Client runs the RFC 8628 device flow: `POST device_authorization_endpoint` → user approves
   in a browser at `verification_uri` → client polls `token_endpoint` until it gets a bearer.
4. Client sends `Authorization: Bearer <token>` on every subsequent request.
5. Client uses **fixed paths under `{base}`** for inference (`/v1/messages`), policy
   (`/managed/settings`), model discovery (`/v1/models`), and telemetry
   (`/v1/{metrics,logs,traces}`).
6. Before expiry, client silently calls `token_endpoint` with `grant_type=refresh_token`.
   No refresh token issued → the user is sent back through the browser flow.

All paths are relative to the `{base}` URL the developer set via `/login`. **The client never
follows cross-origin redirects** and never hard-codes OAuth paths (it reads them from
discovery).

## Endpoint reference

Legend: **R** = required, **O** = optional. Auth column: *none* = unauthenticated,
*bearer* = `Authorization: Bearer <token>`, *browser* = user-facing page.

| Endpoint | Method | Auth | R/O | Purpose |
| :-- | :-- | :-- | :-- | :-- |
| `/.well-known/oauth-authorization-server` | GET | none | R | RFC 8414 AS metadata (discovery) |
| `{device_authorization_endpoint}` | POST | none | R | RFC 8628 §3.2 device authorization |
| `{verification_uri}` | GET/POST | browser | R | User approves the device code against your IdP |
| `{token_endpoint}` | POST | none | R | Device grant + refresh grant (form-encoded) |
| `/v1/messages` | POST | bearer | R | Anthropic Messages API — inference |
| `/v1/messages/count_tokens` | POST | bearer | R¹ | Token counting |
| `/managed/settings` | GET | bearer | O | Per-user `managed-settings.json` |
| `/v1/models` | GET | bearer | O | Model discovery for the `/model` picker |
| `/v1/metrics`, `/v1/logs`, `/v1/traces` | POST | bearer | O | OTLP/HTTP telemetry |

¹ `count_tokens` is listed under the required Messages section; on backends without a
count-tokens API (Bedrock) return `501 not_supported` and the client falls back to a Haiku
`max_tokens:1` probe.

### Discovery — `GET /.well-known/oauth-authorization-server`

RFC 8414 metadata. The client reads only `device_authorization_endpoint` and `token_endpoint`
(both must be **same-origin** with `{base}`) and ignores the rest. `authorization_endpoint` is
**intentionally absent** — this is a device flow, not an authorization-code redirect flow.

```json
{
  "issuer": "https://gw.corp.example.com",
  "device_authorization_endpoint": "https://gw.corp.example.com/oauth/device_authorization",
  "token_endpoint": "https://gw.corp.example.com/oauth/token",
  "grant_types_supported": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"]
}
```

### Device authorization — `POST {device_authorization_endpoint}`

RFC 8628 §3.2. Client opens `verification_uri_complete` in the browser and polls
`token_endpoint` every `interval` seconds.

```json
{
  "device_code": "AbK9-s3n4C8H...",
  "user_code": "WDJB-MJHT",
  "verification_uri": "https://gw.corp.example.com/device",
  "verification_uri_complete": "https://gw.corp.example.com/device?user_code=WDJB-MJHT",
  "expires_in": 600,
  "interval": 5
}
```

`device_code` ≥ 256 bits, opaque, single-use. `user_code` uses a base-20 charset (RFC 8628
§6.1).

### Verification page — `GET/POST {verification_uri}`

Browser-facing; **the client never calls this**. Accept the user code, authenticate against
your IdP, mark the matching `device_code` approved so the next token poll succeeds. Apply a
per-IP rate limit (§5.1); don't auto-submit a pre-filled code (§5.4).

### Token — `POST {token_endpoint}`

Unauthenticated, `application/x-www-form-urlencoded`.

**Device grant** (`grant_type=urn:ietf:params:oauth:grant-type:device_code`):

| Status | Body | Client reaction |
| :-- | :-- | :-- |
| 200 | `{"access_token","token_type":"Bearer","expires_in","refresh_token"?}` | Login complete. `refresh_token` optional; omit → client re-runs device flow on expiry. |
| 400 | `{"error":"authorization_pending"}` | Keep polling. |
| 400/429 | `{"error":"slow_down"}` | Add 5s to the poll interval. |
| 400 | `{"error":"access_denied"}` | Stop. |
| 400 | `{"error":"expired_token"}` | Stop. |

**Refresh grant** (`grant_type=refresh_token`): return a fresh
`{"access_token","token_type","expires_in","refresh_token"}` on 200. Return
`401 {"error":"invalid_grant"}` to force re-login — **this is your deprovisioning hook**.

### Messages — `POST /v1/messages`, `POST /v1/messages/count_tokens`

The Anthropic Messages API, unchanged. Proxy to your upstream and stream the response back.

- Enforce the model allowlist here → `400 invalid_request_error` for a denied model.
- **Don't buffer SSE** on the `stream: true` path.
- Client always sets `Content-Length`, so you may reject chunked-without-CL (`411`) and cap
  body size (`413`).
- Client doesn't assume server-side tools are available.
- Client also sends `x-app` and `x-stainless-*` headers — pass through or drop, but **don't
  reject the request because of them**.

### Managed settings — `GET /managed/settings`

The authenticated user's `managed-settings.json` (see the [settings
reference](https://code.claude.com/docs/en/settings)). Client polls ~hourly; support
`ETag`/`If-None-Match` → `304`. **`404` = "no managed policy"; `200 {}` = "empty policy" — not
the same thing.** Flagged as the endpoint most likely to change.

### Models — `GET /v1/models`

Anthropic models-list shape `{"data":[{"id","display_name"},...]}`. Use Anthropic-style IDs
(`claude-{family}-{major}-{minor}`) — the client's model-family logic keys on that shape. Only
called when `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` is set on the client (deliverable via
the `env` block in `/managed/settings`). `404` → fall back to the client's built-in list.

### Telemetry — `POST /v1/{metrics,logs,traces}`

OTLP/HTTP (protobuf or JSON). When connected to a gateway the client sends telemetry here and
**ignores `OTEL_EXPORTER_OTLP_*` env vars**. **Return `200` whether you forward or discard** —
`404` makes the client's exporter log an error on every flush.

## Error envelopes

OAuth endpoints use `{"error","error_description"}` (RFC 6749/8628). Bearer-authenticated
endpoints use the **Anthropic error envelope** so the SDK surfaces the message:

```json
{"type":"error","error":{"type":"authentication_error","message":"..."}}
```

| HTTP | `error.type` | Use for |
| :-- | :-- | :-- |
| 400 | `invalid_request_error` | Denied model, malformed body, policy violation |
| 401 | `authentication_error` | Missing/expired/invalid bearer; client prompts re-login |
| 403 | `permission_error` | Authenticated but not allowed |
| 413 | `request_too_large` | Body over your cap |
| 429 | `rate_limit_error` | Throttling; include `Retry-After` |
| 501 | `not_supported` | Endpoint not available on this backend |
| 529 | `overloaded_error` | Upstream at capacity; client backs off and retries |
| 5xx | `api_error` | Anything else |

## Bearer token

The `access_token` is **opaque to the client** — stored, sent, refreshed before `expires_in`,
never inspected. Encode the user's identity and groups in the token (or in server-side state
keyed by it) so you can apply per-user RBAC at `/v1/messages` and per-group policy at
`/managed/settings`. The same token must work across every bearer-authenticated endpoint.

## TLS

`https://` required; `http://` accepted **only for loopback** during development. The client
pins the SHA-256 fingerprint of the TLS leaf certificate per-hostname after the user confirms
it on first connect, and re-prompts on mismatch — rotating the cert costs every user one
confirmation prompt.

## Client guarantees (what a gateway can rely on)

- OAuth endpoint paths come from the discovery document; the client never hard-codes
  `/oauth/token`.
- Fixed-path endpoints are resolved against `{base}`, never a redirect.
- Every request body carries `Content-Length`.
- The OTLP exporter is locked to `{base}/v1/{signal}` regardless of the user's environment.
- `404` from `/v1/models` or `/managed/settings` is a clean "not implemented" — no retry storm.

## Proxying to Bedrock, Vertex, or Foundry

Proxying to `api.anthropic.com` is pass-through. A cloud provider's Claude endpoint needs
translation:

- **Model IDs.** Client sends Anthropic-style IDs (`claude-sonnet-4-5`); translate to the
  upstream form (Bedrock model ID or inference-profile ARN; Vertex `@`-versioned ID), or
  advertise upstream-native IDs from `/v1/models`.
- **`anthropic-beta`.** Bedrock rejects some betas in the *header* → move them into the request
  body as `"anthropic_beta": [...]`. Vertex and Foundry accept the header.
- **Streaming.** Bedrock's native stream is AWS binary event-stream, not SSE; decode and
  re-emit Anthropic-shaped `text/event-stream`. Provider SDKs handle this.
- **`count_tokens`.** Bedrock has no count-tokens API → return `501 not_supported`; client
  falls back to a Haiku `max_tokens:1` probe.
- **Headers.** Forward `content-type`, `accept`, `accept-encoding`, `anthropic-version`,
  `anthropic-beta`, `user-agent`, `x-stainless-*`; strip the client's `Authorization` and
  apply the upstream's credentials. On the response, strip hop-by-hop headers
  (`content-encoding`, `content-length`, `transfer-encoding`, `connection`).
- **Errors.** Upstream messages may carry cloud account IDs/ARNs/project IDs — log them for the
  operator, return a generic message, but **keep `error.type`** so the client's retry logic
  still works.

## How this maps onto shunt

shunt implements the `ANTHROPIC_BASE_URL` (Anthropic Messages) gateway surface, not the full
Claude apps gateway OAuth sign-in. The relevant pieces already covered elsewhere:

- **`/v1/messages` + `count_tokens`** — [M1 — Messages ⇄ Responses
  translation](m1-responses-translation.md).
- **`/v1/models` discovery** — [M3 — Model discovery](m3-discovery.md) (same wire contract,
  incl. the `claude`/`anthropic` ID filter).
- **Inbound bearer auth** — [M4 — Inbound client authentication](m4-inbound-auth.md).
- **SSE streaming / no buffering** — [M5 — SSE keepalive](m5-sse-keepalive.md).

The device-flow sign-in (`/.well-known/oauth-authorization-server`, device authorization,
verification page, token endpoint) and `/managed/settings` + `/v1/{metrics,logs,traces}` are
**Claude apps gateway-specific** and not part of shunt's current surface. They are the sections
to consult if shunt ever grows a first-class `/login` flow.

## Conformance & gaps

shunt implements the `ANTHROPIC_BASE_URL` (Anthropic Messages) surface described above. This
comparison was captured 2026-07-13 against `GET /protocol` from a locally-run Claude apps
gateway (Claude Code 2.1.207). Tracked under epic #87.

### Endpoints intentionally omitted

The following six endpoints are part of the Claude apps gateway **superset**, out of scope for
the `ANTHROPIC_BASE_URL` surface, and belong to the separate `/login` device-flow track (see
"How this maps onto shunt" above):

- `GET /.well-known/oauth-authorization-server`, `POST /oauth/device_authorization`,
  `GET`/`POST /device`, `POST /oauth/token`
- `GET /managed/settings`
- `POST /v1/{metrics,logs,traces}` — inbound OTLP *ingest*. Note shunt's `telemetry.rs` is
  outbound OTLP *export*, a different thing.

### Behavioral gaps

| Gap | Severity | Evidence | Tracking |
| :-- | :-- | :-- | :-- |
| `count_tokens` `Estimate` mode returns `404 not_found_error` instead of `501 not_supported` (default `Tiktoken` mode is fine) | Low | `src/proxy.rs:264-271`, `src/config.rs:295-296` | #89 |
| `GET /v1/models` always unauthenticated, even under `[server.auth]` (model-list exposure on shared-key gateways; spec allows optional auth) | Low (posture) | `src/discovery.rs:35-40`, `src/proxy.rs:224-231` | #90 |
| (info) ChatGPT/xAI `401` message replaced with a fixed re-login hint — intentional (`401` → re-login is the recovery) | Info | `src/adapters/responses.rs:599-602` | folded into #88 |

### Confirmed conformant (no action)

- `anthropic-beta`/`anthropic-version` open-list header forwarding on the Anthropic path.
- No SSE buffering.
- Anthropic error-envelope shape on all paths.
- `429` + `Retry-After`.
- `/v1/models` returns the `{"data":[{id,display_name}]}` shape.
- Table-driven model-id remap, including body rewrite.
- On the translated backends (Responses/Codex, xAI, Cursor), upstream
  `403`/`413`/`429`/`500`/`501`/`502`/`503`/`504`/`529` reach the client with their
  preserved status and Anthropic `error.type` (`permission_error`/`request_too_large`/
  `rate_limit_error`/`not_supported`/`overloaded_error`/`api_error`) rather than being
  flattened to `502`/`401`; other unexpected statuses are intentionally normalized to
  `502 api_error` — landed in #88.

Not forwarding `anthropic-beta` to the Responses/Cursor backends is correct, not a gap — that
path is format translation rather than header passthrough, so there is nothing to relay as-is.

This table tracks known gaps under epic #87 and shrinks as #88/#89/#90 land.

## Reproducing the capture

```bash
# 1. Postgres (device-flow + rate-limit state)
docker run -d --name pg -e POSTGRES_USER=gw -e POSTGRES_PASSWORD=pw \
  -e POSTGRES_DB=gateway -p 55432:5432 postgres:16-alpine

# 2. Minimal gateway.yaml — boot is fail-closed on config/Postgres/OIDC/upstream.
#    A public OIDC issuer (e.g. https://accounts.google.com) satisfies discovery;
#    Bedrock creds resolve at first request, not boot, so auth:{} boots fine.
#    listen.public_url: http://localhost:8080  (loopback http allowed for dev)

# 3. Run it (native binary required) and fetch the spec — /protocol is unauthenticated
claude gateway --config gateway.yaml &
curl -s http://127.0.0.1:8080/protocol -o gateway-protocol.raw.md
```

## References

RFC 6749 (OAuth 2.0), RFC 8414 (AS metadata), RFC 8628 (device grant), Anthropic Messages API,
Claude Code settings reference, OTLP spec.
