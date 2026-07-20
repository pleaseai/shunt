# Cross-vendor usage bars — design

Three milestones, in the order the CLI/operator sees value:

- **M-A** — an inbound `GET /api/oauth/usage` synthesizer, so Claude Code's own native
  usage bars can render correctly when `ANTHROPIC_BASE_URL` points at shunt. Fully specified
  below, **contingent on** a pre-implementation CLI-reachability check and explicit
  maintainer sign-off on its new config key and auth model — see "Status" at the top of the
  M-A section. Not pre-approved by virtue of being written down.
- **M-B** — cross-vendor bars inside that same native UI. **Blocked** by a client-side
  allowlist Anthropic controls, not by anything in shunt's response shape — see the
  verdict below. Ships a fallback surface instead (extend [M12](../../m12-client-usage-endpoint.md)'s
  `GET /usage`), and says plainly what remains blocked.
- **M-C** — new per-vendor usage *sources* to feed both M-A's pool and M-B's fallback
  surface. One section per vendor from the recon matrix; most are cut as YAGNI given the
  evidence, one (Codex) is worth a scoped ticket.

This document assumes the code map and recon results supplied to the design session; file
paths and line numbers below refer to the `feat/214-admin-live-activity` tree as surveyed.

**Revision note:** this version incorporates a critique review that found the original
M-A auth model would 401 real Claude Code traffic on shared gateways, the primary
recommended credential path might never trigger the CLI's own fetch, and the reused M12
aggregate is misleading for multi-account pools under native-UI labeling. See the
changelog at the end of this document for the full list of what changed and why.

## M-A — inbound `GET /api/oauth/usage` synthesizer

**Status:** implementation-ready *contingent on* the verification step in
"Precondition" below and the explicit sign-off called out in "Boundaries" —
not pre-approved simply because it is fully specified. (Revised after
external critique; see the changelog at the end of this document for what
changed and why.)

### Problem

Claude Code's CLI renders its own usage bars (`Current session`, `Current week (all
models)`, and — gated by a server-side allowlist — `Current week (Fable)`) by calling
`GET {ANTHROPIC_BASE_URL}/api/oauth/usage` itself (`fetchUtilization`, per static
analysis of the shipped CLI binary, see recon §probe). When `ANTHROPIC_BASE_URL`
points at shunt instead of `https://api.anthropic.com`, that path 404s — shunt registers
no such route today — so the CLI's own bars silently render empty. shunt already computes
a related aggregate for its own `GET /usage` ([M12](../../m12-client-usage-endpoint.md));
M-A reshapes accounts-pool telemetry into the exact wire format the CLI expects, so the
*existing* Claude Code UI can show real numbers when routed through shunt, **for the CLI
login modes that actually call this endpoint** — see Precondition. This is deliberately
narrower than the original framing ("no client-side change, no flag, shows real numbers")
implied: M-A makes the route answer correctly *when called*; it does not by itself
guarantee every documented shunt credential setup causes the CLI to call it.

### Precondition — verify CLI reachability before writing code

The recon's static-analysis claim that `fetchUtilization` fires was not validated against
shunt's own *documented* credential matrix, and a plausible reading of the probe (fetch
gated behind `Wo()`/`pD()`, described elsewhere as OAuth/subscription-login checks) is that
the fetch only happens for a CLI using a full, refreshable `claude login` session — not for
`claude setup-token` or a raw API key, both of which `connect-claude-code.md` documents as
supported (and setup-token as *recommended*) for pointing Claude Code at a gateway. shunt's
own outbound poller doc comment already establishes a related fact in the other direction
(`src/auth/claude/usage.rs`: "a long-lived `claude setup-token` is rejected [by Anthropic's
usage API]"), which is consistent with usage bars being a subscription-login concept — but
is not proof of what triggers the CLI's *inbound* fetch.

Before implementation starts, capture live (or recorded-session/tmux) evidence, under each
of these three credential setups pointed at a throwaway local shunt instance, of whether
`GET /api/oauth/usage` is actually requested:

1. `ANTHROPIC_AUTH_TOKEN` from `claude setup-token` (the README/`connect-claude-code.md`
   recommended path) — no `[server.auth]`.
2. A full interactive `claude login` (Max/Pro subscription) with `ANTHROPIC_BASE_URL` set —
   no `[server.auth]`.
3. A `[server.auth]`-gated shunt with the assigned shunt client token set as
   `ANTHROPIC_AUTH_TOKEN` (the shared-gateway pattern documented in
   `guides/connect-claude-code.md` and `guides/shared-gateway.md`).

Record the outcome in the milestone doc (`docs/m14-oauth-usage-endpoint.md`) before or in
the same PR as the implementation. Decision rule:

- If (2) fetches and (1)/(3) do not: ship M-A scoped explicitly as "works for CLIs using a
  full interactive subscription login; setup-token and API-key logins do not trigger the
  CLI's own fetch, though the route still answers correctly if some other client calls it."
  Say this plainly in the README/guide docs (do not claim "works out of the box" — see
  Docs).
- If none of the three fetch: do not implement the route registration/handler at all;
  file a follow-up against the recon instead, and redirect effort to `GET /usage` (M12) /
  admin, per the critique's own fallback recommendation.
- If (1) or (3) also fetch: the scope note above can be relaxed accordingly, but the auth
  model in this document must still hold (a shared-gateway CLI on path (3) presents its
  Anthropic OAuth bearer to `/api/oauth/usage`, not the shunt client token it presents to
  `/v1/messages` — see Auth gating).

This gate exists because a route that is fully correct but never called by the deployment
it targets is not a shippable feature — it is dead code with a maintenance cost.

### Route and module

- New route: `GET /api/oauth/usage` — the exact path and method the CLI calls, and
  incidentally the exact path shunt's own outbound poller already calls upstream
  (`USAGE_PATH` in `src/auth/claude/usage.rs:19`). The synthesizer is the mirror image of
  that client: same wire shape, opposite direction.
- New module: `src/oauth_usage.rs`, declared `pub mod oauth_usage;` in `src/lib.rs`
  alongside `pub mod usage;`.
- The handler does **not** call `usage::aggregate` — see "Aggregation policy" below for why
  M-A owns its own (small, pure, unit-tested) window-selection function instead of reusing
  M12's pool-wide least-utilized aggregate verbatim. It still reuses M12's per-window
  utilization/reset accessor closures, `round4`-style rounding approach, and non-finite/
  disabled filtering conventions — only the *selection* logic differs. One promotion is
  still needed: `FABLE_SCOPE_DISPLAY_NAME` in `src/auth/claude/usage.rs` must become
  `pub(crate)` so the wire-shape builder can emit the same literal the parser expects,
  instead of re-hardcoding `"Fable"` a second time.

### Wire response shape

Exactly Anthropic's `/api/oauth/usage` schema, as parsed by `src/auth/claude/usage.rs`'s
`parse_usage`/`parse_window`/`parse_fable_window` — this is not a new contract, it's the
mirror of one shunt already consumes:

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

Types (`src/oauth_usage.rs`):

```rust
#[derive(Debug, Default, Serialize, PartialEq)]
struct OauthUsageWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    five_hour: Option<WindowWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seven_day: Option<WindowWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    limits: Vec<LimitWire>,
}

#[derive(Debug, Serialize, PartialEq)]
struct WindowWire {
    utilization: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct LimitWire {
    kind: &'static str,           // always "weekly_scoped"
    scope: LimitScope,
    percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct LimitScope {
    model: LimitScopeModel,
}

#[derive(Debug, Serialize, PartialEq)]
struct LimitScopeModel {
    display_name: &'static str,   // crate::auth::claude::usage::FABLE_SCOPE_DISPLAY_NAME
}
```

### Aggregation policy: two deviations from M12, both required

M12's `GET /usage` aggregate (`usage::aggregate` / `usage::window_status`) is the wrong
default for a route that feeds bars labeled "Current session" / "Current week" — that
labeling reads as *personal* to a user, not "best headroom among N pooled logins." M-A
therefore deviates from M12 in two independent ways, not one:

**Deviation 1 — Claude accounts only.** M12's `GET /usage` deliberately blends
`AuthMode::ClaudeOauth` **and** `AuthMode::ChatgptOauth` accounts into one pool-wide
aggregate (`src/usage.rs:174-177`) — a generic "how healthy is the whole gateway" signal.
M-A must **not** do that: it answers a call the CLI makes to ask about *its own Claude
subscription*, so blending in Codex/ChatGPT utilization would silently misreport a Claude
session's real headroom (and vice versa, for an operator confused about why their Claude
bar tracks Codex traffic). M-A's snapshot loop therefore filters providers to
`AuthMode::ClaudeOauth` only — worth its own regression test (see Tests).

**Deviation 2 — routing-aware, priority-tiered worst case, not pool-wide least-utilized.**
`usage::window_status` reports `1 - min(utilization)` across every non-disabled account,
ignoring `priority`, `available` (cooldown/near-quota), and everything else
`AccountPool::select_order` (`src/accounts.rs`) actually weighs when picking which account
serves the *next* request. Reused verbatim, that produces exactly the failure the critique
flagged: a priority-1 (preferred) account at 95% utilization plus a priority-100 (backup)
account at 5% utilization would report "~5% used" — an optimistic number — while real
traffic keeps hitting the priority-1 account until it is actually exhausted or cooling.
That is not a rounding error, it is the aggregate answering a different question than the
one the label implies.

M-A instead computes, per window, a **routing-aware, priority-tiered worst case** — a
deliberately coarser approximation of `select_order` than reimplementing sticky-session/
round-robin/burn-rate logic, but one that fixes the specific failure mode above:

1. Filter to non-disabled Claude accounts reporting a finite utilization for this window
   (same base filter M12 uses today).
2. Partition that set into `usable` (`AccountSnapshot.available == true` — already encodes
   not disabled, not cooling, not near-quota) and the rest.
3. Prefer `usable` if non-empty; otherwise fall back to the full non-disabled set (mirrors
   `select_order`'s real behavior of still routing to a near-quota/cooling account when
   nothing else is left, rather than reporting an artificially rosy "no usable accounts ⇒
   empty" bar for a pool that is, in practice, exhausted).
4. Within whichever set step 3 selected, take the accounts at the **lowest `priority` value**
   present (the most-preferred tier `select_order` tries first).
5. Within that tier, report the **maximum utilization** (worst case) — the tier is exactly
   the set of accounts round-robin/sticky selection can hit for the *next* request, so the
   bar should reflect the least headroom among them, not the most.
6. `resets_at` comes from whichever account within the tier was selected in step 5.

This lives as a new pure function in `src/oauth_usage.rs` (not `src/usage.rs` — M12's
handler, aggregate, and tests are untouched; see Boundaries), generic over the same
`utilization`/`reset` accessor-closure shape `usage::window_status` already uses, so the
five-hour/seven-day/Fable call sites are one-liners each:

```rust
fn routing_aware_window(
    snapshots: &[AccountSnapshot],
    utilization: impl Fn(&AccountSnapshot) -> Option<f64>,
    reset: impl Fn(&AccountSnapshot) -> Option<u64>,
) -> Option<(f64, Option<u64>)> {
    let candidates: Vec<&AccountSnapshot> = snapshots
        .iter()
        .filter(|s| !s.disabled)
        .filter(|s| utilization(s).is_some_and(f64::is_finite))
        .collect();
    let usable: Vec<&&AccountSnapshot> =
        candidates.iter().filter(|s| s.available).collect();
    let pool: Vec<&AccountSnapshot> = if usable.is_empty() {
        candidates
    } else {
        usable.into_iter().copied().collect()
    };
    let min_priority = pool.iter().map(|s| s.priority).min()?;
    pool.into_iter()
        .filter(|s| s.priority == min_priority)
        .map(|s| (utilization(s).expect("filtered above"), reset(s)))
        .max_by(|(a, _), (b, _)| a.total_cmp(b))
}
```

This is intentionally **not** a general-purpose reimplementation of `select_order` (no
sticky-session hashing, no burn-rate headroom tie-break, no cooldown countdown) — it is the
minimum priority/availability awareness needed to stop the aggregate lying about which
account governs the next request. If a future ticket wants tighter routing fidelity, this
function is the place to extend, not `usage::window_status` (M12's contract — pool-wide,
identity-blind — is intentionally simpler and stays that way).

*Considered and rejected:* the critique's other two options — "single-account only" (omit
windows entirely once >1 Claude account is configured) and "pure worst-case across the
whole pool" (ignore priority, take max utilization over every non-disabled account). Single-
account-only throws away the feature for the most common multi-account use case (an
operator running several of their own Claude logins to raise combined throughput) even
though a priority-tiered answer is available and cheap. Pure worst-case-ignoring-priority
would make a *healthy* pool's bar track a low-priority, rarely-used backup account sitting
at high utilization for unrelated reasons (e.g. an operator manually testing on it) even
though the pool would never actually route there first — trading one misleading direction
for another. Priority-tiered worst case is the one option of the three that keys off the
same signal (`priority`) the real router keys off, without pretending to replicate the
rest of the router.

### Inverse-mapping rules (fraction/epoch → percent/RFC3339)

The new `routing_aware_window(..) -> Option<(f64, Option<u64>)>` (utilization fraction,
resets epoch) maps to the wire shape as:

- `utilization` (or `percent` for the Fable limit) = `round2(utilization.clamp(0.0, 1.0) *
  100.0)`. Clamping defensively even though `note_quota`/`note_codex_quota` already write
  fractions in `0.0..=1.0` in practice — this is a response-shape boundary, not an
  internal invariant, so it re-asserts the range rather than trusting upstream never to
  regress it. `round2` is a new one-line helper (`(value * 100.0).round() / 100.0`).
- `resets_at` (string) = `epoch.map(|secs| format_iso8601(UNIX_EPOCH + Duration::from_secs(secs)))`,
  reusing `crate::auth::shared::format_iso8601` (`src/auth/shared.rs:473`, already
  `pub(crate)`) rather than writing a second epoch→RFC3339 formatter next to the RFC3339→epoch
  one already in `src/auth/claude/usage.rs`. Both halves of the round trip then live in the
  codebase exactly once each.
- A window where `routing_aware_window` returns `None` (no non-disabled Claude account
  reports it, or none has a finite `priority` — i.e. never, since `priority` is always
  populated, but the `None` case exists for "zero qualifying accounts") **omits the key**
  (`skip_serializing_if = "Option::is_none"`) rather than emitting a fabricated `0%`. This
  is not an error — it is the same "no signal" semantics M12 already established
  (`docs/m12-client-usage-endpoint.md`, "Codex is blank by design"): the CLI's renderer
  already handles an absent/incomplete window by not drawing that bar.
- The Fable limit entry is included in `limits` **only** when the Fable window's
  `routing_aware_window` call returns `Some`; otherwise `limits` stays empty and is omitted
  entirely (`skip_serializing_if = "Vec::is_empty"`).
- shunt tracks exactly one Fable-scoped weekly bucket (`7d_oi`/`QuotaState.utilization_7d_oi`);
  it cannot synthesize a second `weekly_scoped` entry for whatever "Fable 5" (a second name
  also seen in the client's live allowlist, see M-B) would need — that would require widening
  `QuotaState`/`UsageSnapshot` first and is out of scope for M-A. Likewise `seven_day_sonnet`
  (a third, conditionally-rendered bar in the CLI) has no backing bucket anywhere in
  `accounts.rs` and is never emitted. **Boundary, stated explicitly for the implementer:**
  this endpoint emits only the fields the CLI's own renderer draws today
  (`five_hour`, `seven_day`, one `weekly_scoped` limit named `"Fable"`); if a future
  GrowthBook change widens the CLI's renderer to more names, that is a reason to revisit
  this endpoint, not something shunt can pre-empt from the server side.

### Auth gating (rewritten — the original design's model does not match real CLI traffic)

**Why the original "same as `/v1/models`" model is wrong here.** `GET /v1/models`'s optional
composed check (`InboundAuth::authenticate_client`) validates the presented credential
*against the configured `[server.auth]` client-token list*. That works for `/v1/models`
because a client pointed at a gated shunt is expected to send the shunt-assigned client
token. But the credential Claude Code actually presents on `GET /api/oauth/usage` is
whatever bearer the CLI already uses for its own outbound Anthropic calls — for a full
`claude login` session, that is the operator's live **Anthropic OAuth access token**, not a
shunt client token, even on a gateway where `[server.auth]` is configured and the operator
correctly set `ANTHROPIC_AUTH_TOKEN` to their assigned shunt token for *inference* traffic
(`ANTHROPIC_AUTH_TOKEN` and the CLI's internal OAuth-session bearer are not always the same
value — see Precondition, path (2) vs (3)). Validating that bearer against the configured
token list would 401 exactly the traffic this endpoint exists to serve.

**Revised model — gated by bind topology, not by credential matching:**

- **Loopback bind** (`server.bind_addr()?.ip().is_loopback()` — the common personal
  deployment, README's own quickstart default of `127.0.0.1:3001`): serve **unauthenticated**,
  regardless of whether `[server.auth]`/`[server.gateway]` happen to be configured for other
  reasons. The request cannot have originated off the operator's own machine, so there is
  no third party to gate against; requiring a credential match here would only recreate the
  401 failure mode above for zero security benefit.
- **Non-loopback bind:** `[server.oauth_usage]` requires at least one of `[server.auth]` or
  `[server.gateway]` to be configured — enforced at config validation (new `ConfigError`,
  see Config) so a public/shared gateway cannot silently expose pool quota telemetry with no
  gate at all (this is the fix for "unauthenticated telemetry is not `/v1/models`-equivalent
  discovery precedent" — quota utilization is materially more sensitive than a model-ID
  list). At request time, admit the request if **any** credential-shaped value is present in
  the dedicated `[server.auth]` header, `x-api-key`, or `Authorization: Bearer` —
  **regardless of whether it matches a configured token**. Log whether the presented value
  matched a configured client token (`identified`) or not (`unverified — anthropic oauth
  bearer assumed`) at `info`/`debug` respectively, but do not 401 on a non-match. Reject
  (`401`) only when **none** of the three slots carries any value at all.

  This is a deliberate, narrower gate than `/v1/models`'s: it stops a fully anonymous
  scrape (no credential presented at all) while not 401ing the CLI's real, valid-but-
  unmatchable OAuth bearer. The residual exposure is explicit and must be documented, not
  silently accepted: on a non-loopback `[server.oauth_usage]` deployment, *any* caller
  holding *some* credential for the gateway — a valid shunt client token, a gateway JWT, or
  even an arbitrary non-empty Bearer value copied from somewhere else — can read the
  sanitized, identity-free pool aggregate this endpoint returns. That is an acceptable
  trade for "the feature does not 401 the CLI it exists to serve" only because the response
  itself is already sanitized (see the existing `aggregate_never_exposes_account_identity_
  or_capacity`-style invariant, carried over unchanged) — it never leaks account names,
  counts, or priorities, only pool-wide percentages. Operators who need stricter enforcement
  should keep `[server.oauth_usage]` loopback-only (e.g. behind their own reverse proxy that
  does real bearer verification before forwarding) rather than exposing it directly.

Implementation: new `src/oauth_usage.rs`-local function (not a copy of `discovery.rs`'s
strict composed check, since the semantics now differ) —

```rust
fn is_loopback(state: &AppState) -> bool {
    state
        .config
        .server
        .bind_addr()
        .is_ok_and(|addr| addr.ip().is_loopback())
}

fn has_any_credential(auth_header: Option<&HeaderName>, headers: &HeaderMap) -> bool {
    let has = |name: &HeaderName| headers.get(name).is_some_and(|v| !v.as_bytes().is_empty());
    auth_header.is_some_and(has)
        || has(&axum::http::header::AUTHORIZATION)
        || headers
            .get("x-api-key")
            .is_some_and(|v| !v.as_bytes().is_empty())
}
```

### Config

New opt-in table, presence-as-opt-in (matches `[server.usage]`/`[server.admin]`/
`[server.codex_endpoint]`):

```toml
[server.oauth_usage]
```

```rust
/// Optional opt-in inbound `GET /api/oauth/usage` synthesizer for Claude Code's
/// own native usage bars (see `docs/m14-oauth-usage-endpoint.md`). Absent ⇒ the
/// route is not registered (today's HTTP surface unchanged, the path 404s as it
/// does now). Auth is bind-topology-gated, not credential-matched — see the
/// milestone doc for why (the CLI's own Anthropic OAuth bearer, not a shunt
/// client token, is what actually arrives on this route).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub oauth_usage: Option<OauthUsageConfig>,
```

```rust
/// `[server.oauth_usage]` — presence alone opts in; no fields today (mirrors
/// `UsageEndpointConfig`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct OauthUsageConfig {}
```

Two new `ConfigError` variants, both checked in `Config::validate` alongside the existing
`UsageEndpointRequiresAuth` check (`src/config.rs:2115`):

```rust
#[error("[server.oauth_usage] on a non-loopback [server.bind] requires [server.auth] or [server.gateway]: without one, Claude subscription quota telemetry would be served to any caller on the network")]
OauthUsageEndpointRequiresAuthOnNonLoopback,

#[error("providers.{provider} (claude_oauth) base_url resolves to this gateway's own [server.bind] with [server.oauth_usage] enabled: the outbound usage poller would read back its own synthesized aggregate instead of Anthropic's real usage")]
OauthUsageSelfPollLoop { provider: String },
```

```rust
// Independent of any single provider — checked once, alongside the existing
// `UsageEndpointRequiresAuth` check (`src/config.rs:2115`).
if self.server.oauth_usage.is_some() {
    let non_loopback = self
        .server
        .bind_addr()
        .is_ok_and(|addr| !addr.ip().is_loopback());
    if non_loopback && self.server.auth.is_none() && self.server.gateway.is_none() {
        return Err(ConfigError::OauthUsageEndpointRequiresAuthOnNonLoopback);
    }
}
```

The self-poll-loop guard lives *inside* the existing per-provider `AuthMode::ClaudeOauth`
block (`src/config.rs:1961-1981`), which already parses `url` (via `self.provider_base_url`,
line 1912) and computes `host` — reuse both rather than re-parsing:

```rust
if provider.auth == AuthMode::ClaudeOauth {
    if provider.kind != ProviderKind::Anthropic {
        return Err(ConfigError::ClaudeOauthWrongKind { provider: name.clone() });
    }
    let host = url.host_str().unwrap_or_default();
    if !host_is_loopback(host) {
        // ...existing https/anthropic.com checks, unchanged...
    } else if self.server.oauth_usage.is_some() {
        // A loopback claude_oauth base_url is allowed to be "any host" so a local
        // debugging proxy or mock can receive the bearer (see the comment already
        // on this branch) — but if it happens to land on this gateway's own bind
        // port with the usage synthesizer enabled, the outbound poller would read
        // back its own synthesized aggregate instead of Anthropic's real usage.
        if let Ok(bind) = self.server.bind_addr() {
            let port = url.port_or_known_default().unwrap_or(0);
            if port == bind.port() {
                return Err(ConfigError::OauthUsageSelfPollLoop {
                    provider: name.clone(),
                });
            }
        }
    }
}
```

This guard (raised by critique review) is a same-loopback-interface, same-port heuristic,
not an exhaustive topology check (it does not resolve DNS names or account for a reverse
proxy sitting between the poller and shunt) — it exists to catch the realistic mistake
(copy-pasting shunt's own address into a `claude_oauth` provider's `base_url` instead of a
genuine debugging-proxy address), not every conceivable indirection. Document the general
rule in prose too (see Docs): never point a `claude_oauth` provider's `base_url` at this
gateway.

Router registration (`src/server.rs`, alongside the existing `usage_enabled` boolean):

```rust
let oauth_usage_enabled = config.server.oauth_usage.is_some();
// ...
if oauth_usage_enabled {
    router = router.route("/api/oauth/usage", get(oauth_usage::get));
}
```

Boot-time decision, same as every other opt-in route table — a reload cannot add or drop the
route. Add the matching entry to `warn_on_restart_only_changes` in `src/reload.rs` (after the
existing `[server.usage]` check, `src/reload.rs:126-130`):

```rust
if previous.server.oauth_usage.is_some() != next.server.oauth_usage.is_some() {
    tracing::warn!(
        "[server.oauth_usage] was enabled or disabled but requires a restart to register or drop its route"
    );
}
```

No `AppState` changes are required. The handler only needs `state.accounts`, `state.config`,
`state.inbound_auth`, and `state.gateway_auth` — all already present and already
`refreshed()`-able exactly as `usage::get` uses them today.

### Handler sketch

```rust
pub async fn get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let state = state.refreshed();
    if !is_loopback(&state) {
        let auth_header = state.inbound_auth.as_ref().map(|a| a.header().clone());
        if !has_any_credential(auth_header.as_ref(), &headers) {
            tracing::warn!(
                "inbound auth failed for GET /api/oauth/usage: no credential presented on a non-loopback bind"
            );
            return ShuntError::new(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "missing credential: this gateway requires a client token, gateway login, or an Anthropic OAuth bearer to read usage",
            )
            .into_response();
        }
        // Presence is enough; log whether it also happens to match a configured
        // client token, but do not gate on the match (see Auth gating).
    }
    let config = state.config.clone();
    let accounts = state.accounts.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut snapshots = Vec::new();
        for (name, provider) in &config.providers {
            if provider.auth != AuthMode::ClaudeOauth {
                continue; // Codex/Cursor/etc. never contribute to this endpoint.
            }
            let resolved = /* same resolve-or-scan-store logic as usage::get */;
            snapshots.extend(accounts.snapshot(name, &resolved, None, config.server.pool.as_ref()));
        }
        Ok(snapshots)
    }).await;
    match result {
        Ok(Ok(snapshots)) => Json(to_wire(&snapshots)).into_response(),
        Ok(Err(())) => ShuntError::new(StatusCode::INTERNAL_SERVER_ERROR, "api_error", "failed to read pool usage").into_response(),
        Err(join_error) => { /* same panicked-task handling as usage::get */ }
    }
}
```

`to_wire(&[AccountSnapshot]) -> OauthUsageWire` calls `routing_aware_window` three times
(5h/7d/fable) and builds the wire struct — it replaces the old `to_wire(&UsageResponse)`
signature now that M-A no longer routes through `usage::aggregate`.

The store-scan/spawn_blocking/error-shape structure is copied verbatim from `usage::get`
(`src/usage.rs:157-231`) — same reasons (blocking file I/O and a `std::sync::Mutex` lock
must not run on an async worker), same failure classification (store I/O failure is a `500`,
never silently "empty pool, full headroom").

### Error shapes

All gateway-owned errors use the Anthropic error shape (`ShuntError`), matching every other
route in this file except the inbound Codex endpoint (AGENTS.md's documented exception,
which does not apply here — this route serves an Anthropic-protocol client):

| Condition | Status | `type` | Notes |
| :-- | :-- | :-- | :-- |
| Non-loopback bind, no credential in any of the three slots | `401` | `authentication_error` | New message (not copied from `discovery.rs` — the semantics differ, see Auth gating) |
| No `ClaudeOauth` provider configured, or zero accounts resolved | `200` | — | **Not an error.** Body omits every window/limit (`{}`), matching "no account reports the window ⇒ omitted" |
| Account store scan I/O failure | `500` | `api_error` | Mirrors `usage::get`'s `"failed to read pool usage"` |
| Snapshot task panicked | `500` | `api_error` | Mirrors `usage::get` |
| `[server.oauth_usage]` on non-loopback bind with neither `[server.auth]` nor `[server.gateway]` | *(startup failure, not a request)* | — | `ConfigError::OauthUsageEndpointRequiresAuthOnNonLoopback`; shunt refuses to boot rather than register the route unauthenticated |
| `claude_oauth` provider `base_url` resolves to this gateway's own bind with `[server.oauth_usage]` enabled | *(startup failure, not a request)* | — | `ConfigError::OauthUsageSelfPollLoop` |

### Tests

1. `to_wire`/`routing_aware_window` unit tests: `Some`/`None` → key present/omitted; percent
   rounding (`round2`) on representative fractions; a mixed-priority set where the top tier
   wins regardless of a lower-priority account's utilization.
2. **Priority-tiered worst-case regression test (explicitly required):** one `priority: 1`
   account at 95% utilization and one `priority: 100` account at 5% utilization for the same
   window → response reflects the `priority: 1` account's 95%, not the 5% a pool-wide
   least-utilized aggregate would have reported. This is the direct regression test for the
   failure mode that motivated Deviation 2.
3. **Availability-fallback test:** all non-disabled accounts in the top priority tier are
   `available: false` (cooling/near-quota) → the function still returns a value (falls back
   to the full non-disabled set per step 3), rather than `None`/omitted, for a pool that is
   in practice still routable-to, just degraded.
4. **Round-trip self-consistency**, placed in `src/auth/claude/usage.rs`'s own test module
   (not `oauth_usage.rs` — `parse_window`/`parse_fable_window`/`parse_rfc3339_to_epoch_secs`
   are private `fn`s there and are **not** promoted to `pub(crate)` just for this test;
   `oauth_usage::to_wire` is instead marked `pub(crate)` so `auth::claude::usage`'s test
   module can call it): build an `AccountSnapshot`, run it through `oauth_usage::to_wire`,
   serialize, then feed the resulting `serde_json::Value` back through this module's own
   `parse_window`/`parse_fable_window` and assert the recovered utilization/`resets_at`
   match the input. Do the same epoch round trip through `format_iso8601` →
   `parse_rfc3339_to_epoch_secs`.
5. Handler test: no `ClaudeOauth` provider configured → `200` with an empty-object body.
6. **Sanitization-invariant test** (explicitly required — mirrors
   `aggregate_never_exposes_account_identity_or_capacity`, `src/usage/tests.rs:103`):
   assert the serialized response never contains an account `name`, `priority`, `disabled`,
   `threshold`, `headroom`, `cooldown`, or shunt's own `status` field.
7. **Provider-filter regression test** (Deviation 1): configure one `ClaudeOauth` account at
   low utilization and one `ChatgptOauth` (Codex) account at high utilization; assert the
   response reflects only the Claude account's numbers.
8. Auth tests (new model, not `discovery.rs`'s): loopback bind, no auth configured at all →
   `200` unauthenticated. Loopback bind, `[server.auth]` configured, no credential presented
   → still `200` (loopback bypasses the gate entirely). Non-loopback bind, `[server.auth]`
   configured, a well-formed but *non-matching* Bearer presented → `200` (admitted,
   unverified). Non-loopback bind, `[server.auth]` configured, no credential in any of the
   three slots → `401`.
9. Config-validation tests: `[server.oauth_usage]` alone on a loopback bind validates
   successfully (no auth required). `[server.oauth_usage]` on a non-loopback bind with
   neither `[server.auth]` nor `[server.gateway]` → `OauthUsageEndpointRequiresAuthOnNonLoopback`.
   A `claude_oauth` provider whose `base_url` is `http://127.0.0.1:<bind-port>` with
   `[server.oauth_usage]` enabled and `[server.bind]` on the same loopback port →
   `OauthUsageSelfPollLoop`.
10. `OauthUsageConfig` serde round trip (mirrors `UsageEndpointConfig`, `src/config.rs:2312-2355`).
11. Router test mirroring `usage_route_is_registered_and_answers_when_enabled_with_valid_auth`
    (`src/server.rs:280`): `/api/oauth/usage` 404s when `[server.oauth_usage]` is absent, and
    answers when present.
12. `reload.rs` test mirroring the existing `[server.usage]` toggle-warning test: toggling
    `[server.oauth_usage]` presence across a reload logs the restart-only warning and the
    reload still succeeds.

### Docs to update in the same PR (per AGENTS.md doc-drift rule)

- `README.md` — new paragraph alongside the existing "Opt-in client usage endpoint" one
  (`README.md:82`), explaining `[server.oauth_usage]` makes Claude Code's *own* `/usage`
  command work when pointed at shunt, **stating the exact precondition proven in the
  Precondition section** (which login modes actually trigger the CLI's fetch) rather than
  claiming universal "works out of the box" coverage. Contrast with `[server.usage]`, which
  is a separate, shunt-native, sanitized aggregate for arbitrary clients.
- `docs/m14-oauth-usage-endpoint.md` — new milestone note (next free number; `m13` is
  reserved by the concurrent `docs/m13-admin-activity-surface.md` live-activity work on this
  branch). Same sections as `docs/m12-client-usage-endpoint.md`: whose usage, contrast with
  `GET /usage` and `GET /admin/pool`, configuration, response, boundaries (explicitly:
  Claude-only, priority-tiered worst-case not pool-wide optimistic aggregate, no
  `seven_day_sonnet`, single Fable bucket only, only the fields the CLI's own renderer
  draws), auth model (bind-topology-gated, not credential-matched — spell out the residual
  exposure on non-loopback deployments), and the recorded outcome of the Precondition
  verification.
- `site/src/content/docs/reference/configuration.md` — new `## [server.oauth_usage] (optional)`
  section next to the existing `## [server.usage] (optional)` one (line 139), including the
  loopback-vs-non-loopback auth distinction and the self-poll-loop warning below.
- `site/src/content/docs/reference/endpoints.md` — new endpoint entry for
  `GET /api/oauth/usage`.
- `site/src/content/docs/guides/anthropic-multi-account.md` — note the exact preconditions
  (login type, single-vs-multi-account aggregation behavior) instead of "works out of the
  box"; add: never point a `claude_oauth` provider's `base_url` at this gateway's own bind —
  doing so with `[server.oauth_usage]` enabled makes the outbound usage poller read back its
  own synthesized aggregate (also enforced at config validation, see Config).
- `wiki/` — not touched (generated).

### Boundaries

- ⚠️ This introduces a new public config key (`[server.oauth_usage]`) **and** a new
  authentication model that deliberately does not verify the presented credential's
  membership on non-loopback binds (see Auth gating) — both need explicit maintainer
  sign-off before implementation starts, not just before merge. Do not treat this design
  document as that sign-off; it specifies the contract, not the approval.
- 🚫 Does not touch `[server.usage]`, its handler, its aggregate, or its tests — M-A's
  routing-aware window selection is new code in `src/oauth_usage.rs`, not a variant of
  `usage::window_status`.
- The endpoint is Claude-only by design; Codex/Cursor accounts never contribute (see the
  provider-filter test above). This is the point, not a gap.
- The Precondition verification (live/recorded evidence of which credential setups actually
  trigger the CLI's fetch) is a **blocking prerequisite**, not a nice-to-have — implementing
  the route without it risks shipping dead code for the primary documented credential path
  (`claude setup-token`).

## M-B — cross-vendor bars: blocked in the native UI, fallback surface instead

### Verdict from the recon probe

The recon's renderer probe (`renders_extra_limits: "no"`) is decisive, not speculative: the
CLI's `limits[]` renderer (`Wxt`, decompiled) does iterate the array generically by
`kind`/`scope.model.display_name`, but only survives entries whose `display_name` is in an
allowlist read from a **GrowthBook remote feature flag**
(`tengu_usage_overage_included_models`), fetched and cached independently of
`ANTHROPIC_BASE_URL`. The probe confirmed this machine's own cached flag value is exactly
`["Fable", "Fable 5"]` — nothing else. A shunt response cannot add "Codex Weekly" or "Grok 5h"
to that list; the gate is data Anthropic controls server-side, not code shunt's response can
influence. This holds regardless of what M-A's `limits[]` contains.

**Decision: do not attempt to render cross-vendor bars inside Claude Code's native `/usage`
view.** Any `limits[]` entry M-A (or a future extension of it) emits with a `display_name`
outside that allowlist is silently dropped by the client before it ever reaches the screen —
shipping such entries would be dead weight in the response, not a feature.

### Fallback surface: per-provider sections on `GET /usage`

The surface shunt *does* control the renderer for is its own [M12](../../m12-client-usage-endpoint.md)
`GET /usage` — consumed by an operator's own dashboard, monitoring script, or (per the M-A
sibling ticket, out of scope here) shunt's admin dashboard. Today that endpoint already has
a latent correctness issue worth fixing as part of this work: its aggregate blends
`ClaudeOauth` and `ChatgptOauth` accounts into one pool-wide `5h`/`7d`/`fable` set
(`src/usage.rs:174-177`), which is misleading once a deployment runs both — a Codex account
at 90% utilization can make a healthy Claude pool report 10% remaining, and vice versa.

Proposed shape (additive, backward compatible — existing `pool` key unchanged in
computation; its meaning is now documented explicitly rather than left implicit — see
below):

```json
{
  "pool": { "status": "ok", "windows": { "5h": {...}, "7d": {...}, "fable": {...} } },
  "providers": {
    "anthropic":     { "status": "ok", "windows": { "5h": {...}, "7d": {...}, "fable": {...} } },
    "codex-primary": { "status": "degraded", "windows": { "5h": {...}, "7d": {...} } }
  }
}
```

- `providers` keys are the configured provider names (same identifiers as `[[providers]]`
  table keys elsewhere in config output — not account names, so the M12 sanitization
  invariant is preserved: still no per-account identity, just a coarser per-provider
  bucket than the single pool-wide one). **Resolved (was an open question):** provider
  name, not `AuthMode`, because keying by `AuthMode` (`anthropic`/`chatgpt`) would silently
  re-blend a deployment running *two* `claude_oauth` providers (e.g. two independently
  configured Claude pools) back into one bucket — reintroducing, one level down, the exact
  blending bug this ticket exists to fix. Provider names are operator-chosen config labels,
  not account secrets; they are already implicitly visible to any caller with the
  `[server.auth]` token this endpoint already requires (e.g. via error messages that name
  the provider). Leaking them to an authenticated caller is a materially smaller concern
  than the M12 sanitization invariant (no *account* name/priority/threshold/headroom), which
  this change does not touch.
- Each provider's `windows` reuses `usage::window_status`/`WindowStatus` verbatim, computed
  from that provider's own snapshot slice instead of the concatenated one — again, reuse, not
  a new aggregation. (Unlike M-A, this fallback surface is not labeled as anyone's personal
  "current session" — it is explicitly a pool-wide operator dashboard, so M12's optimistic
  least-utilized aggregate is the right semantic here, scoped per provider instead of
  per-pool. M-A's routing-aware/priority-tiered deviation does not apply to this surface.)
- `fable` is omitted (not `null`) for a provider whose `AuthMode` has no Fable concept
  (Codex) — a structural omission is a clearer signal than a `null` a caller might mistake
  for "no accounts reported this window right now."
- Gate this addition behind the *existing* `[server.usage]` opt-in — it is the same
  sanitized-transparency feature, one field richer. No new config table.
- **`pool`'s documentation, not its computation, changes:** strengthen `PoolStatus`'s doc
  comment and `docs/m12-client-usage-endpoint.md` to say plainly that `pool` is a coarse,
  cross-backend "governing worst status" signal once `providers` exists, and that a caller
  wanting per-backend accuracy should read `providers` instead. *Considered and rejected:*
  changing `pool`'s own computation (either to Claude-only or to an explicit worst-case-
  across-providers definition) in this same change — `pool` is an existing, unversioned,
  already-shipped response field; redefining what it measures is a breaking change for any
  existing consumer parsing it today, and does not need to happen in the same PR that adds
  the strictly additive `providers` object. If `providers` availability makes `pool` mostly
  vestigial in practice, deprecating (not redefining) it is a separate, explicit follow-up
  decision for a maintainer, not something to fold into this fallback-surface ticket.

This is an additive, non-breaking change to an existing response, so it does not by itself
need a new milestone number — track it as a follow-up ticket against
`docs/m12-client-usage-endpoint.md` (see `issues.md`).

## M-C — new usage sources

One section per vendor from the recon matrix. "Feasibility" is the recon's own verdict, not
re-litigated here.

### Codex (ChatGPT/OpenAI) — feasible, worth a scoped ticket

shunt already ingests Codex's header-based signal fully (`note_codex_quota`,
`src/accounts.rs:458-514`) — that part needs no new work. The recon's genuinely new finding
is that codex-rs (OpenAI's own CLI source) independently confirms a **pollable** usage
endpoint backing the Codex CLI's own `/status` display: `GET {base}/wham/usage`
(ChatGPT-OAuth path) or `GET {base}/api/codex/usage` (API-key path), same Bearer +
`ChatGPT-Account-Id` credential shunt already injects for Codex. This is the same shape as
[M8](../../m8-anthropic-multi-account.md)'s Claude usage poller: headers already give shunt a
per-request signal, but a poller additionally reconciles *out-of-band* consumption (the
operator's own interactive Codex CLI usage happening outside shunt) — exactly
`src/usage_poll.rs`'s stated rationale, just for the second backend.

The endpoint also exposes `additional_rate_limits[]`, a genuinely new signal (per-model/
per-feature caps) with no analog in shunt's current `QuotaState` shape at all — that part is
a bigger, separate design question (does it need its own bucket type, does the pool's
threshold/burn-rate selection logic need to understand per-model caps) and should not block
shipping the narrower primary/secondary reconciliation poller. Scope the ticket to the
narrow win; file the `additional_rate_limits[]` question as an explicitly separate open
question, not a blocking sub-task.

### xAI/Grok (SuperGrok) — feasible only via an unofficial, reverse-engineered endpoint

Official `api.x.ai` rate limits are per-model RPS/TPM by spend tier with no documented
remaining%/reset signal at all — not useful here. The one endpoint that reports what this
feature needs (a weekly SuperGrok credit-pool percent + reset) is
`POST grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig`, an undocumented gRPC-web
protobuf call reverse-engineered by third-party tooling, authenticated primarily by browser
session cookies (shunt holds neither cookies nor a browser session for its xAI accounts —
only the OAuth device-flow tokens in `~/.shunt/xai-auth.json`). Building a poller against an
undocumented protobuf contract with a different credential shape than shunt's existing xAI
auth is a materially different risk profile than the Codex ticket above (official, documented,
same credential shunt already holds). Worth filing as a proposal so a maintainer can decide
the risk tradeoff explicitly, but not designed further here — the open questions (protobuf
schema stability, cookie-vs-OAuth-token auth, whether xAI ships an official replacement) are
for that discussion, not this document.

### Kimi (Moonshot) — feasible only via an unofficial, generic-provider-shaped endpoint

Kimi is configured in shunt purely as a generic `kind = "anthropic"` provider — there is no
Kimi-specific adapter or account-pool wiring to extend (`src/config.rs`, no built-in Kimi
entry). The recon's endpoint (`GET {base}/usages` on the Kimi Code Coding-Plan base URL,
`Bearer sk-kimi-*`) is well-documented from the official `kimi-cli` open-source client — more
concrete than the xAI signal — but consuming it would require shunt to grow account-pool
quota tracking for an arbitrary user-configured "anthropic-kind" provider, which is a
different shape of feature than anything shunt does today (quota tracking is currently
built into the Claude- and Codex-specific code paths, not generic). File as a proposal
scoped narrowly to "does a generic-provider quota poller belong in shunt at all," not as an
implementation-ready ticket — that architectural question has to be answered before any
endpoint-parsing details matter.

### Gemini (incl. Antigravity) — cut, not designed

Per the task framing: Gemini would first require shunt to become a Gemini/Antigravity
provider at all (`src/config.rs`'s `ProviderKind` has no Gemini variant, and no adapter
exists) — quota tracking is not meaningfully separable from that larger, out-of-scope
decision. Independently, the only non-empty *quota* signal the recon found (Antigravity's
local Connect-RPC `GetUserStatus`) is unofficial, requires a live Antigravity process
co-resident on the same host as the poller, and authenticates via a CSRF token scraped from
that process's `ps` output — a fundamentally different trust/credential model than every
other provider shunt polls. Not designed further; note only.

### Ollama — cut (YAGNI, feasibility: none)

Ollama Cloud returns no rate-limit headers and no quota field in any response body
(confirmed by an open, unanswered upstream issue, `ollama/ollama#15663`). The only reported
signal is an HTML scrape of the account-settings *web page*, authenticated by a browser
session cookie — a credential type shunt's adapters do not hold for any provider today (every
existing adapter holds an API key or OAuth token, never a browser cookie). shunt also has no
native Ollama adapter at all (`ProviderKind` has no Ollama variant); an operator would have to
route it through the generic OpenAI-Responses path today. Cut: there is no signal to build
against that fits shunt's existing credential model, and no adapter to attach it to.

## Changelog — critique review

This document was revised after an external critique review (blocker/major findings applied;
minors applied where cheap; the rest resolved with an explicit rationale in place). Summary
of what changed, mapped to the critique's numbering:

| # | Severity | Finding | What changed |
| :-: | :-- | :-- | :-- |
| 1 | blocker | Shared-gateway auth would 401 real Claude Code traffic (composed check validated the wrong credential) | Auth model rewritten: bind-topology-gated (unauthenticated on loopback, "any credential present" — not credential-*matched* — on non-loopback) instead of `/v1/models`'s strict composed check. See M-A "Auth gating." |
| 2 | blocker | The primary recommended credential path (`setup-token`) may never trigger the CLI's own fetch at all | Added a blocking "Precondition" subsection requiring live/recorded verification across the three documented credential setups before implementation starts, with an explicit decision rule (including "don't implement" if none fetch). Overclaiming language in the Problem statement removed. |
| 3 | major | Unauthenticated telemetry on this route is more sensitive than `/v1/models` discovery, and wasn't gated like M12 | New `ConfigError::OauthUsageEndpointRequiresAuthOnNonLoopback`: `[server.oauth_usage]` on a non-loopback bind now fails startup unless `[server.auth]` or `[server.gateway]` is configured. Loopback stays the safe, ungated default (matches the primary target deployment). |
| 4 | major | Reusing M12's pool-wide least-utilized aggregate is misleading for native "Current session/week" labeling in a multi-account pool | M-A no longer calls `usage::aggregate`. New "Aggregation policy" section adds a second, explicit deviation from M12: a routing-aware, priority-tiered worst-case window selection (own pure function in `oauth_usage.rs`), with the rejected alternatives (single-account-only, pure pool-wide worst-case) recorded inline. |
| 5 | major | Docs plan claimed "works out of the box" while the design is opt-in/gated/multi-account-ambiguous | "Docs to update" rewritten to require stating the exact verified precondition and aggregation semantics instead of "out of the box" language, in README, the milestone doc, and the multi-account guide. |
| 6 | major | Round-trip test plan targeted private (non-crate-visible) parser functions | Round-trip test moved into `src/auth/claude/usage.rs`'s own test module (where the private fns it needs are already visible); only `oauth_usage::to_wire` is marked `pub(crate)` for that test to call, instead of promoting three parser internals. |
| 7 | major | "Claude-only" framing addressed only the cross-vendor half of the misleading-aggregate problem, not the cross-*account* half | Folded into the #4 fix: the priority-tiered aggregation change is Deviation 2, explicitly separate from and additional to the Claude-only provider filter (Deviation 1). Added the exact regression test the finding asked for (priority-1 exhausted + priority-100 fresh). |
| 8 | minor | M-B's `pool` key keeps blending providers with no plan to stop that being a permanent footgun; provider-name-vs-`AuthMode` question left open | Open question resolved explicitly in favor of provider-name keys (with a stated reason: `AuthMode` keying would re-blend multi-pool-per-backend deployments). `pool`'s *documentation* strengthened to state its coarse, cross-backend meaning and point callers at `providers`; changing `pool`'s own computation was considered and explicitly deferred as a separate, breaking-change decision — see the "Considered and rejected" note in M-B. |
| 9 | minor | Speculative/YAGNI cuts (SuperGrok, Kimi, non-allowlisted `limits[]`, Codex `additional_rate_limits[]`) | No change — the critique confirmed these were already correctly cut; re-verified, left as-is. |
| 10 | minor | Design framed itself as pre-approved ("implementable without re-deciding anything") despite introducing a new public config key and (now) a non-standard auth model | Removed that framing everywhere it appeared (top summary, M-A heading). Added an explicit "Status" line and a Boundaries bullet stating the new config key *and* the new auth model both need maintainer sign-off before implementation starts, not just before merge. |
| 11 | minor | Outbound Claude usage poller could self-hit M-A's route if a `claude_oauth` provider's `base_url` were misconfigured to point at shunt's own bind | Implemented (not just documented) — new `ConfigError::OauthUsageSelfPollLoop`, a same-loopback-interface/same-port heuristic added to the existing per-provider `ClaudeOauth` validation branch, plus a doc callout. Chose the cheap heuristic over a full DNS/proxy-aware topology check, since the realistic failure mode is a copy-paste mistake, not adversarial routing — noted inline as a scoping decision, not a full fix for every conceivable indirection. |
| 12 | minor | Wire-shape gaps (missing `seven_day_sonnet`, `"Fable 5"`, other `limits[]` kinds) needed an explicit "only what the CLI renders today" boundary statement | Added directly to the Inverse-mapping rules section and to the milestone-doc bullet in "Docs to update." |

No finding was rejected outright; #8, #9, and #11 were accepted with a scoped
implementation (documented above and inline at each site) rather than the critique's exact
literal wording, for the stated reasons.
