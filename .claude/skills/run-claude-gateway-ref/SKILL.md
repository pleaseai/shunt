---
name: run-claude-gateway-ref
description: Launch and drive a local reference Claude apps gateway (claude gateway + Dex + Postgres) to probe, capture, or re-verify the login / managed-settings / OTLP-telemetry wire protocol that shunt's gateway superset (login → managed/settings → telemetry, epic #87) must implement. Use to run the reference gateway, walk the device flow non-interactively, capture wire fixtures, or observe the telemetry relay.
---

# Run the reference Claude apps gateway (protocol probe)

Spins up the **real** `claude gateway` (built into the `claude` binary) with a
local Dex IdP and throwaway Postgres, then drives the full device-flow login
**non-interactively with curl** — no browser needed. Use it to (re)capture the
wire contract shunt mirrors: exact token JSON, `/managed/settings` body, env
push, OTLP relay behavior.

Everything is driven by `.claude/skills/run-claude-gateway-ref/driver.sh`
(paths relative to repo root). Backend note: curl is sufficient — the `/device`
browser page is scriptable once you send same-origin CSRF headers.

## Prerequisites

- Docker running (pulls `postgres:16-alpine`, `ghcr.io/dexidp/dex:v2.44.0`)
- `claude` binary ≥ 2.1.207 (verified with 2.1.211), `python3`, `curl`
- Ports free: 8790 (gateway), 5556 (dex), 55432 (pg), 44318 (sink)

## Run (agent path)

```bash
S=.claude/skills/run-claude-gateway-ref/driver.sh
$S up          # pg + dex containers, gateway on :8790, waits healthy
$S sink-start  # optional: local OTLP sink so the telemetry relay is observable
$S login       # full RFC 8628 device flow via curl → capture/token.json
$S probe       # captures protocol.md, managed_settings.json (+304), models.json,
               # refresh grant, POST /v1/metrics relay, /user/bootstrap
$S status      # health of all three processes
$S down        # kill gateway + sink, remove containers
```

Artifacts land in `/tmp/shunt-gw-probe/capture/` (override root with
`GWREF_WORK`). Gateway log: `/tmp/shunt-gw-probe/gateway.log` (audit events are
JSON lines — `evt`: `device.authorize`, `device.verify`, `session.mint`,
`session.refresh`, `managed.serve`, `auth.denied`). Sink log:
`/tmp/shunt-gw-probe/sink.log`.

Login identity: `dev@example.com` / `password` (Dex static user, no groups).

## Verified wire facts (2026-07-16, gateway 2.1.211)

Captured live; these fill the gaps `docs/gateway-protocol.md` marks unspecified:

- Token response (device grant AND refresh grant, same shape):
  `{"access_token":"<HS256 JWT>","refresh_token":"<opaque>","token_type":"Bearer","expires_in":3600}`.
  JWT claims: `sub` (IdP subject), `email`, `name`, `aud`=oidc client_id,
  `iss`=public_url. Refresh **rotates** the refresh token. Gateway refresh =
  refresh against the IdP (3500ms budget) + re-mint; IdP failure → `401 {"error":"invalid_grant"}`.
- `/managed/settings` body: `{"uuid":"sha256:…","checksum":"sha256:…","settings":{<managed-settings.json>}}`,
  `ETag` = checksum, `If-None-Match` → 304. With `telemetry.forward_to` set it
  pushes **six** env vars (docs say five): `CLAUDE_CODE_ENABLE_TELEMETRY=1`,
  `OTEL_{METRICS,LOGS,TRACES}_EXPORTER=otlp`, `OTEL_EXPORTER_OTLP_ENDPOINT=<public_url>`,
  `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, merged with policy `env`.
- Discovery includes undocumented `gateway_protocol_version: 1`,
  `token_endpoint_auth_methods_supported: ["none"]`, `scopes_supported`.
- IdP callback is `{public_url}/oauth/callback` (register in the IdP client).
- Telemetry relay is **verbatim** — body forwarded byte-identical, no identity
  attributes stamped by the gateway; `POST /v1/{metrics,logs,traces}` returns
  200 even when the destination is down. Boot log says `signals enabled: metrics`
  for a bare `forward_to` entry.
- `/v1/models` is filtered by the caller's policy `availableModels`; full
  Anthropic shape (`type`, `has_more`, `first_id`, `last_id`).
- Error envelope adds top-level `request_id` alongside `{"type":"error","error":{…}}`.
- `GET /user/bootstrap` (new, Claude Desktop): 404 `not_found_error` unless a
  policy carries a `desktop:` block.

## Gotchas

- **`POST /device` is CSRF-guarded**: without `Origin` + `Referer` +
  `Sec-Fetch-Site: same-origin` it returns 200 with "request came from another
  site and was blocked" instead of the 302 to the IdP. The audit log shows
  `device.verify result=csrf_rejected`.
- **The `gw_dev` cookie from the `/device` POST must be replayed on
  `/oauth/callback`** or the device grant is never approved.
- **Dex `latest` (master) hangs forever on refresh** (memory AND sqlite3
  storage) → gateway `session.refresh` fails with "timed out after 3500ms" and
  the client gets `invalid_grant`. Pin `v2.44.0`.
- Loopback OIDC issuer is rejected by the SSRF guard unless
  `CLAUDE_GATEWAY_ALLOW_LOOPBACK=1` (driver sets it). `http://` public_url is
  accepted on loopback.
- Don't let `curl -w` write into a JSON capture file — it corrupts the JSON and
  the next `python3 json.load` fails with "Extra data".
- The dummy `upstreams` API key boots fine; only `/v1/messages` inference would
  fail. Postgres is required at boot (gateway runs 6 migrations).

## Troubleshooting

- `refresh grant: 401` + gateway log "timed out after 3500ms" → your Dex image
  is unpinned/master; `docker rm -f shunt-gw-dex` and rerun `up` (pulls v2.44.0).
- `device POST did not redirect` from the driver → CSRF headers missing
  (driver sends them; check you didn't proxy/strip them).
- Boot fails on config: unknown keys fail fast with a field-level error; check
  `/tmp/shunt-gw-probe/gateway.log` first.
