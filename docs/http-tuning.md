# HTTP tuning

## Scope

The `[server]` HTTP tuning tables control inbound admission, request sizes,
upstream response-header waits, and the two unauthenticated device-flow rate
limits. They apply independently of provider routing.

The default inference request-body limit changes from the previous hardcoded 64
MiB cap to 32 MiB. Anthropic Messages and inbound Codex Responses requests
between those sizes now receive `413 request_too_large` unless you raise
`[server.limits] max_request_bytes`. Other gateway, admin, telemetry, and
analytics routes retain their endpoint-specific body limits.

## Configuration

```toml
[server.access_control]
allow_cidrs = ["10.0.0.0/8"]
deny_cidrs = ["10.20.0.0/16"]
trust_forwarded_for = false

[server.limits]
max_request_bytes = 33554432
max_request_header_bytes = 65536
max_url_length = 8192

[server.timeouts]
upstream_ttfb_ms = 120000

[server.rate_limits.device_authorization]
max = 30
window_seconds = 600

[server.rate_limits.device_verify]
max = 10
window_seconds = 600

[server.rate_limits.exchange]
max = 60
window_seconds = 600

[server.rate_limits.exchange_failure]
max = 10
window_seconds = 600
```

## Access control

`deny_cidrs` is evaluated first. A matching deny entry rejects the client even
when an allow entry also matches. A non-empty `allow_cidrs` list makes all other
addresses default-deny. `/` and `/health` bypass only the allow list; deny rules
still apply to both liveness routes. If the connection has no peer address, a
non-empty allow list rejects the request.

By default shunt uses the connection peer address. Set `trust_forwarded_for =
true` only when shunt runs behind a trusted proxy that overwrites
`X-Forwarded-For` and `X-Real-IP`. Otherwise a client can spoof these headers to
bypass an address rule. CIDRs are parsed during configuration validation, so an
invalid entry makes `shunt check` fail.

This switch is independent of `[server.gateway] trust_forwarded_for`. The
access-control switch resolves client addresses only for CIDR allow/deny rules;
the gateway switch resolves client addresses for the device-flow rate limiters.
The access-control switch also decides the client address for the token-exchange
failure limit (see "Token-exchange rate limits"); the gateway switch does not.
When both surfaces run behind a trusted reverse proxy, set both switches. Setting
only one leaves the other surface using the socket peer address.

Changing this table requires a restart because its middleware is installed when
the router is built.

## Request limits

`max_request_bytes` defaults to 33,554,432 bytes (32 MiB) for Anthropic
Messages and inbound Codex Responses requests. A declared `Content-Length`
above the limit is rejected before shunt reads the body. Chunked requests
without a declared length use the same limit while the body is read. Both paths
return `413 request_too_large`. Other gateway, admin, telemetry, and analytics
routes retain their endpoint-specific body limits. This key is read from the
per-request configuration snapshot and hot-applies after reload.

`max_request_header_bytes` is optional. Its measurement is the sum of each
parsed header name length and header value length; it does not include HTTP wire
framing. `max_url_length` is also optional and measures the request URI string,
including its query string. These two limits are boot-fixed middleware settings,
so changing either requires a restart. They return `431` and `414`, respectively.

## Upstream response-header timeout

`upstream_ttfb_ms` defaults to 120,000 ms. `0` disables it. The timeout wraps
only the wait for the upstream HTTP response headers. After headers arrive, the
response body has no wall-clock cap, so a long-running SSE response continues to
stream.

The timeout covers these inference HTTP sends:

- Anthropic Messages, including its single-credential and pooled Claude OAuth
  paths through the shared Anthropic send function.
- OpenAI Responses HTTP transport, including the HTTP fallback from the Codex
  WebSocket transport.
- Gemini HTTP transport.
- The inbound Codex Responses passthrough HTTP send.

It does not cover the Codex WebSocket handshake or turn, Cursor's HTTP/Connect
transports, Antigravity local-process execution, discovery probes, OAuth/login
requests, usage polling, OIDC calls, or telemetry relay. A timeout returns a
shunt-owned `504 timeout_error` and is not currently retried.

This key is read per request and hot-applies after reload.

## Device-flow rate limits

The device authorization endpoint and `/device` `user_code` submissions use
independent per-IP fixed-window limiters. Their defaults are 30 requests per 600
seconds and 10 requests per 600 seconds, respectively. The tables are inert when
`[server.gateway]` is not configured. The limiter stores are created at boot, so
changing these values requires a restart.

## Token-exchange rate limits

`POST /v1/oauth/token` (`[server.auth.exchange]`) has two limiters, configured as
optional tables that merge per field with their defaults. `max` and
`window_seconds` must be greater than zero, so neither limit can be switched off.

- `[server.rate_limits.exchange]` (default 60 per 600 seconds) caps successful
  exchanges per verified identity. The identity is the value the matching
  `[[server.auth.jwt]]` entry extracts, counted per entry: two entries on one
  issuer have separate counts, and so do two identities from one address.
- `[server.rate_limits.exchange_failure]` (default 10 per 600 seconds) caps failed
  attempts per client address: a wrong `Content-Type`, an unreadable, oversized or
  malformed body, wrong field types or a missing assertion, an unsupported
  `grant_type`, or an assertion no opted-in entry verifies. A successful
  exchange, a `503` because a key set could not be fetched, and an identity-limit
  `429` are not counted. Once an address is over the limit, its requests are
  refused before the body is read or any assertion is verified, even a valid one.

Over either limit the endpoint answers `429` with
`{"error":"slow_down","error_description":"Too many requests. Wait before retrying."}`,
`Retry-After` set to the whole seconds until that key's window resets, and
`Cache-Control: no-store`. Both limits give the same body; only `Retry-After` can
differ.

The tables apply whenever `[server.auth.exchange]` is configured, with or
without `[server.gateway]`. Counters are memory-only and lost on restart; each
limiter tracks at most 4,096 keys and sweeps idle ones. At capacity a new identity
is refused with `429`; a new address can still exchange a valid assertion, but
its failures are answered `429` without being tracked. A refused key has no window
of its own, so these capacity refusals set `Retry-After` to the full window. The
client address is the TCP peer, or the first `X-Forwarded-For` value and then
`X-Real-IP` when `[server.access_control] trust_forwarded_for` is `true`. A
reload that removes `[server.auth.exchange]` still answers `404`, even for a blocked address. The
limiter stores are created at boot, so changing these values requires a restart.

Gateway-owned errors use the Anthropic error envelope. On inbound Codex paths,
the same access and request-limit errors use the OpenAI Responses envelope.
