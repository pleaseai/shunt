# M14 — Claude Code CLI native usage-bar synthesizer (`GET /api/oauth/usage`)

M14 adds an opt-in, read-only inbound endpoint that mirrors Claude Code CLI's own usage-bar
fetch. The Claude Code CLI renders its own usage bars (`Current session`, `Current week (all
models)`, and — gated by a server-side allowlist — `Current week (Fable)`) by calling
`GET {ANTHROPIC_BASE_URL}/api/oauth/usage` itself. When `ANTHROPIC_BASE_URL` points at shunt
instead of `https://api.anthropic.com`, that path 404s today — shunt registered no such route
before this milestone — so the CLI's own bars silently rendered empty. M14 reshapes
accounts-pool telemetry into the exact wire format the CLI expects, so the CLI's *own,
unmodified* usage UI can show real numbers when routed through shunt, **for the CLI login
modes that actually call this endpoint** — see "Precondition" below. This is deliberately
narrower than "works out of the box for every documented shunt credential setup": M14 makes
the route answer correctly *when called*; it does not by itself guarantee every credential
setup causes the CLI to call it.

See `docs/plans/usage-bars/DESIGN.md` for the full design rationale, the critique that shaped
this final contract, and the rejected alternatives.

## Precondition — which CLI login modes trigger the fetch

Before implementation, the design required live/recorded verification, under each of three
documented shunt credential setups, of whether the CLI actually issues `GET
/api/oauth/usage`:

1. `ANTHROPIC_AUTH_TOKEN` from `claude setup-token` (no `[server.auth]`) — **verified**: does
   **not** fetch. A non-interactive Claude Code CLI session pointed at a local mock server via
   `ANTHROPIC_BASE_URL`, with `ANTHROPIC_AUTH_TOKEN` set to a synthetic, clearly-fake token and
   no login session present, made six requests to `/v1/messages` and zero requests to
   `/api/oauth/usage` across repeated retries.
2. A full interactive `claude login` (Max/Pro subscription), no `[server.auth]` — **not
   safely verifiable in this environment**. Attempting to simulate a logged-in session by
   writing a synthetic `.credentials.json` into a throwaway `CLAUDE_CONFIG_DIR` did not work
   as a safe probe: the CLI's bearer resolution fell back to the machine's real macOS Keychain
   entry rather than honoring the throwaway config directory, so the request the CLI actually
   sent carried what appeared to be a live production OAuth token rather than the synthetic
   one written to disk. That attempt was aborted immediately (process killed, logs deleted)
   once the mismatch was noticed, precisely because a genuine `claude login` requires
   real browser-based OAuth that cannot be safely scripted in a sandboxed session without
   risking exactly this kind of Keychain fallback. This path is therefore **presumed** —
   not directly observed — to trigger the fetch, based on the design recon's static analysis
   of the shipped CLI binary (the fetch is gated behind checks described as OAuth/
   subscription-login detection) and circumstantial UI evidence (a `/usage` panel appears in
   the CLI's own settings surface, seemingly only once a subscription login is present). A
   maintainer with a real Claude subscription should confirm this path directly before or
   shortly after this lands, ideally via a packet capture rather than log inspection.
3. A `[server.auth]`-gated shunt with the assigned shunt client token set as
   `ANTHROPIC_AUTH_TOKEN` (the shared-gateway pattern) — **inferred, not separately tested**:
   mechanically identical to path (1) from the CLI's point of view (the CLI only ever sees an
   opaque bearer in `ANTHROPIC_AUTH_TOKEN`; it cannot distinguish a shunt client token from a
   real setup-token), so it is expected to behave identically: no fetch.

Per the design's decision rule ("if (2) fetches and (1)/(3) do not"), this milestone ships
scoped explicitly as: **works for CLIs using a full interactive subscription login;
`setup-token` and shared-gateway client-token logins do not trigger the CLI's own fetch**,
though the route still answers correctly if some other client (a script, a future CLI version,
a different tool) calls it directly. Do not represent this as "works out of the box" for every
supported shunt credential setup — see README.md and the multi-account guide for the
user-facing wording of this same caveat.

## Contrast with `GET /usage` (M12) and `GET /admin/pool` (M9)

| | `GET /admin/pool` (M9) | `GET /usage` (M12) | `GET /api/oauth/usage` (this milestone) |
| :-- | :-- | :-- | :-- |
| Caller | Operator (browser/admin token) | Any `[server.auth]` client | Claude Code CLI itself (or any client hitting the same path the CLI does) |
| Auth | `[server.admin]` | `[server.auth]` client token | Bind-topology-gated — see "Auth gating" |
| Providers included | All | All | **Claude (`claude_oauth`) only** |
| Aggregation | Per account, full detail | Pool-wide least-utilized | **Routing-aware, priority-tiered worst case** — see "Aggregation policy" |
| Wire shape | shunt's own admin JSON | shunt's own `pool`/`windows` JSON | **Anthropic's own `/api/oauth/usage` schema** (the CLI parses it as such) |

M14 deliberately does not reuse `usage::aggregate` (M12's pool-wide least-utilized aggregate)
for two reasons — both required, both deviations from M12:

**Deviation 1 — Claude-only.** M12's pool aggregate spans every configured provider
(Anthropic, Codex/ChatGPT, Cursor, Grok...). The CLI is asking specifically about its own
Claude subscription; blending in a different backend's utilization would misreport it. M14's
handler filters to `AuthMode::ClaudeOauth` providers only, before any aggregation happens.

**Deviation 2 — routing-aware, priority-tiered worst case, not pool-wide least-utilized.** A
pool-wide least-utilized aggregate is *optimistic*: a priority-1 (preferred) account at 95%
utilization plus a priority-100 (backup) account at 5% would report "~5% used", while real
traffic keeps hitting the priority-1 account until it is actually exhausted or cooling. That
is not a rounding error — it answers a different question than the one the CLI's own label
("Current session") implies. M14 instead computes, per window:

1. Filter to non-disabled Claude accounts reporting a finite utilization for this window.
2. Partition into `usable` (`AccountSnapshot.available == true`) and the rest; prefer
   `usable` if non-empty, otherwise fall back to the full non-disabled set (mirrors
   `select_order`'s real behavior of still routing to a near-quota/cooling account when
   nothing else is left).
3. Within whichever set step 2 selected, take the accounts at the lowest `priority` value
   present (the most-preferred tier `select_order` tries first).
4. Within that tier, report the maximum utilization (worst case) — the tier is exactly the
   set of accounts round-robin/sticky selection can hit for the *next* request.

This lives as `routing_aware_window` in `src/oauth_usage.rs` — a deliberately coarser
approximation of `select_order` (no sticky-session hashing, no burn-rate tie-break, no
cooldown countdown), not a general-purpose reimplementation of it. `GET /usage` (M12) is
unchanged: it keeps its pool-wide, identity-blind, cross-provider contract for its own
audience.

## Configuration

A new opt-in `[server.oauth_usage]` table, mirroring the `[server.usage]` (M12) and
`[server.codex_endpoint]` (M11) presence-as-opt-in pattern. The table has no keys today.

```toml
[server.oauth_usage]
```

The route is registered once at boot when the table is present; a config reload only
re-resolves the auth it gates against on a non-loopback bind (see "Auth gating"), it cannot
add or drop the route.

Two config-validation guards:

- **`ConfigError::OauthUsageEndpointRequiresAuthOnNonLoopback`** — `[server.oauth_usage]` on a
  non-loopback `[server.bind]` requires `[server.auth]` or `[server.gateway]` to be configured.
  Without one, Claude subscription quota telemetry would be served to any caller on the
  network; shunt fails closed at boot rather than expose it.
- **`ConfigError::OauthUsageSelfPollLoop`** — a `claude_oauth` provider whose `base_url`
  resolves to this gateway's own `[server.bind]` host/port, with `[server.oauth_usage]`
  enabled, is rejected. Without this guard, shunt's own outbound usage poller could end up
  reading back its own synthesized aggregate instead of Anthropic's real usage. This is a
  same-loopback-interface, same-port heuristic (it does not resolve DNS names or account for a
  reverse proxy in between) — it exists to catch the realistic mistake of copy-pasting shunt's
  own address into a `claude_oauth` provider's `base_url`, not to be an exhaustive topology
  check.

## Auth gating

This endpoint's auth model is **bind-topology-gated**: unauthenticated on a loopback bind,
and validated exactly like the gated Messages routes on a non-loopback bind.

- **Loopback bind** (`127.0.0.1`/`::1`, shunt's own default): served **unauthenticated**. The
  request cannot have originated off the operator's own machine, so this is the same trust
  boundary shunt's other loopback-only affordances already rely on (see the `claude_oauth`
  provider's loopback `base_url` allowance in `src/config.rs`). This is also the *only* bind
  topology under which the Claude Code CLI actually calls this path: the CLI issues the usage
  fetch only for a subscription OAuth login talking to `ANTHROPIC_BASE_URL`, and never when an
  `ANTHROPIC_AUTH_TOKEN` (the shared-gateway client-token pattern) is set.
- **Non-loopback bind**: requires a **valid** credential — a configured `[server.auth]` client
  token (via its header, `x-api-key`, or `Authorization: Bearer`) **or** a valid gateway JWT —
  exactly as `proxy::check_inbound_auth` gates `/v1/messages`. Mere header *presence* is not
  enough: a fabricated `Authorization: x` must not read pool quota telemetry off a
  network-facing bind. There is no unverifiable-Anthropic-bearer caller to accommodate here,
  precisely because the CLI never fetches this path on a non-loopback shared-gateway bind (see
  the loopback note above). A non-loopback deployment without `[server.auth]`/`[server.gateway]`
  configured at all fails to boot (`ConfigError::OauthUsageEndpointRequiresAuthOnNonLoopback`),
  so a validator is always present.

## Response

Exactly Anthropic's own `/api/oauth/usage` schema — the same shape `src/auth/claude/usage.rs`
already parses as shunt's *outbound* usage poller, in the opposite direction:

```json
{
  "five_hour": { "utilization": 42.37, "resets_at": "2026-07-20T23:00:00Z" },
  "seven_day": { "utilization": 61.02, "resets_at": "2026-07-27T00:00:00Z" },
  "limits": [
    {
      "kind": "weekly_scoped",
      "scope": { "model": { "display_name": "Fable" } },
      "percent": 12.5,
      "resets_at": "2026-07-27T00:00:00Z"
    }
  ]
}
```

`five_hour`/`seven_day` are omitted (not `null`) when no non-disabled Claude account reports
that window; `limits` is omitted entirely (not an empty `[]`) when no account reports the
Fable-scoped window (`#[serde(skip_serializing_if = "Vec::is_empty")]` on the wire struct).
`utilization`/`percent` are fractions rounded to two decimals and multiplied by 100;
`resets_at` is RFC3339 (`crate::auth::shared::format_iso8601`, the same formatter shunt's
outbound client parses via `parse_rfc3339_to_epoch_secs`).

Gateway-owned errors (a `401` for a missing credential on a non-loopback bind, a `500` if the
account store cannot be read) use the Anthropic error shape, like the rest of the gateway.

## Boundaries

- **Sanitization is a test-enforced invariant**, like M12's: a unit test asserts the
  serialized response never contains an account `name`, `priority`, `disabled`, `threshold`,
  `headroom`, `cooldown`, or shunt's own `status` field.
- **Claude-only, routing-aware, not pool-wide optimistic aggregate.** See "Deviation 1" and
  "Deviation 2" above; this is a deliberate, tested divergence from `GET /usage` (M12), not an
  oversight.
- **`GET /usage` (M12) is unchanged.** M14 does not call `usage::aggregate` and does not modify
  M12's handler, tests, or wire shape.
- **The Precondition caveat is load-bearing, not decorative.** Do not describe this feature as
  working "out of the box" for every documented shunt credential setup — see "Precondition"
  above for exactly which login modes trigger the CLI's own fetch.
