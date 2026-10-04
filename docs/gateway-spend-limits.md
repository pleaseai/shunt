# Gateway spend limits

shunt stores per-user and organization spend caps behind an authenticated Admin API, meters what each principal spends on `/v1/messages`, and refuses a principal that has reached a cap with a `429 billing_error`. The sections below cover [configuration](#configuration), [enforcement](#enforcement), [metering](#metering), [rate-limit headers](#rate-limit-headers), [counter persistence](#counter-persistence), the [Admin API](#admin-api) and the [effective spend endpoint](#get-effective), and finish with what is [not yet implemented](#not-yet-implemented).

## Configuration

`[server.spend]` is a top-level section, not a child of `[server.gateway]`. It holds **policy only** — no key material. The endpoints authenticate with the `[server.admin]` credential, so a deployment that never serves gateway login can still administer spend limits; `[server.spend]` without `[server.admin]` fails configuration validation.

```toml
[server.spend]
blocked_message = "Request an increase from FinOps."
audit_retention_days = 365
spend_retention_months = 13
identity_retention_days = 90
group_limit_mode = "min"
# Omit state_path to use the default under $HOME/.shunt, or set an absolute path.
# state_path = "/home/you/.shunt/gateway-spend.json"

[server.spend.enforcement]
fail_closed_on_error = false
```

`state_path = ""` keeps caps and audit records in memory only. Omit `state_path` to use `$HOME/.shunt/gateway-spend.json`; an explicitly configured path is used literally, without shell-style `~` expansion. When shunt cannot resolve a home directory, the default path also becomes memory-only. The state file uses a versioned JSON envelope and an atomic private-file replacement. At restore, shunt parses caps and audit records independently. If a cap or audit snapshot is malformed, fails validation, contains fields that the running version would discard, or uses a scope that the running version does not recognize, shunt logs a warning and carries the complete record through subsequent saves at its original list position. Carry-through caps remain hidden from list, get, and delete operations; carry-through audit records remain outside the stage 1 in-memory audit view. A rollback therefore preserves additive fields and scope variants without blocking startup or rewriting those records into an older schema. Invalid top-level JSON or an unsupported state version still aborts startup so a later mutation cannot overwrite an unreadable envelope. The path is fixed at boot; configuration reloads do not move the process-lifetime store to a different file.

`blocked_message`, `enforcement.fail_closed_on_error` and `spend_retention_months` are live: the first two shape the refusals described under [Enforcement](#enforcement), and the third prunes the meter's counters (see [Counter persistence](#counter-persistence)). `audit_retention_days`, `identity_retention_days` and `group_limit_mode` are parsed for configuration compatibility only: no audit or identity retention sweep runs and no group limit is resolved. `state_path` also locates the counters file (`<state_path stem>.counters.json`).

### Pricing

`[server.spend.pricing]` states what a request costs. The section is optional; omitting it means the built-in list prices at multiplier 1.

```toml
[server.spend.pricing]
multiplier = 0.85            # optional, default 1

[[server.spend.pricing.overrides]]
upstream = "bedrock-eu"      # must name a configured upstream
model = "claude-sonnet-4-6"
input = 3.30                 # USD per million tokens
output = 16.50
cache_read = 0.33
cache_write = 4.125
```

`multiplier` scales every resolved rate, list price and override alike, and models a discount: it must be a finite number of at least `0.000001` and at most `1`. Each override row supplies all four rates in USD per million tokens, and each must be finite, at least `0.001`, and at most `18,446,744,073` (above that the femto-USD conversion saturates). Both floors exist because rates are stored as whole femto-USD (1e-15 USD) per token: their product is exactly 1 femto-USD per token, the smallest nonzero rate the meter can carry, so every accepted rate and multiplier together price at least 1 femto-USD per token. A value below its floor is rejected because, combined with a small enough value on the other side, it rounds down to `0` and prices requests at nothing while looking like a valid discount. A missing rate is a parse error; an out-of-range rate, an out-of-range multiplier, a blank `upstream`, an `upstream` that names no configured upstream (the error lists the ones that exist), and two rows pricing the same model on one upstream all fail configuration validation at boot. Two rows collide when they name the same model, not merely the same string: `claude-sonnet-4-6` and `claude-sonnet-4-6-20260217` are one model on one upstream. A row whose `model` is neither a built-in nor a model that any `[[models]]`, `[[routes]]`, or `[[route_prefixes]]` entry can request **on that row's own `upstream`** is a warning rather than an error — the row is kept, but nothing will ever match it. The check is per upstream and covers prefix routes, matching them case-insensitively even though routing itself is case-sensitive (a prefix shadowed by an earlier prefix routed elsewhere, one that is a case-exact prefix of it, does not count, because routing takes the first match) — a row matches the request model case-insensitively, so it is reachable whenever *any* spelling of its model routes to that upstream, and the client picks the spelling: a model mapped only as another upstream's `upstream_model` does not make the row usable, while a model served by a `[[route_prefixes]]` entry on the row's own upstream does. A row on `server.default_provider` is never warned about, because routing sends everything no route or prefix claimed to that provider.

Model ids are normalized before matching: Claude Code's `[1m]` context-window hint, a Bedrock region prefix and `anthropic.` namespace (including hyphenated regions such as `us-gov.`), a Bedrock `-v<major>:<minor>` version suffix, and a dated snapshot suffix in either the Anthropic (`-20260217`) or Vertex (`@20251101`) form are all stripped. The OpenRouter and Vercel AI Gateway form is normalized too: the `anthropic/` namespace is stripped and the dotted version is hyphenated, so `anthropic/claude-opus-4.8` prices as `claude-opus-4-8`. OpenRouter's floating aliases (`~anthropic/claude-sonnet-latest`) name no version and stay unpriceable.

Rates are matched most-specific-first, for one upstream at a time:

1. an override row whose `model` equals the **upstream** model id (case-insensitively);
2. otherwise an override row whose `model` equals the **client** model id;
3. otherwise an override row naming the same built-in model by a different id — a dated snapshot (`claude-opus-4-5@20251101`), a Bedrock id (`us.anthropic.claude-sonnet-4-6-20260217-v1:0`) — matched against the upstream model first, then the client model;
4. otherwise the built-in list price of the **upstream** model — never of the client model, so a built-in client id remapped to a non-Anthropic upstream model is not billed at Anthropic rates;
5. otherwise nothing: the request cannot be priced, and the meter decides what that means.

The built-in list-price catalog is the fallback, in USD per million tokens, and covers the Claude models shunt routes to. Server-side web search is priced per request ($0.01) rather than per token, and override rows never change it; only the multiplier applies.

All amounts are USD **estimates**. They are computed from the token counts an upstream reports against published list prices, not read back from a provider invoice, so they will not reconcile to the cent with a bill.

The meter prices every metered request through this table (see [Metering](#metering)). A model the table cannot price is charged at the unknown-model rate, `$5 / $25 / $0.50 / $6.25` per million input / output / cache-read / cache-write tokens, scaled by `multiplier` like any other rate, and shunt logs one warning per model id per process.

### Credentials

The credential comes from `[server.admin]`, which resolves three sets — the legacy `name:token` pairs plus two key arrays:

```toml
[server.admin]
# header = "x-shunt-admin-token"     # default; `x-api-key` is accepted too
# tokens_env = "SHUNT_ADMIN_TOKENS"  # default; legacy `name:token` pairs, write tier

[[server.admin.write_keys]]
id = "terraform"
key = "${SHUNT_ADMIN_KEY_TERRAFORM}"

[[server.admin.read_keys]]
id = "reporting"
key = "${file:/run/secrets/shunt-reporting-key}"
```

- **Access tiers.** `read < write`, and `write` implies `read`. A read credential passes every `GET` on the admin and spend surfaces and is refused on every mutation. The `tokens_env`/`tokens_file` `name:token` pairs are the **write** tier, retained for compatibility; new deployments should prefer the arrays, which carry a per-credential `id` for the audit trail. A credential's privilege is the maximum over every set it matches, so the order the sets are scanned in cannot change it.
- **Slots.** A credential is accepted in the configured `[server.admin] header` (`x-shunt-admin-token` by default) **or** in `x-api-key` — on the admin and spend routers only. `x-api-key` is the caller's own Anthropic credential slot on inference routes, where an admin credential never authenticates anything. A request may fill both slots; privilege is then the higher of the two, and when the two are different credentials of the same tier the configured header is the one the audit trail records. Whatever these routes accept is also stripped from that slot before any upstream request, so an admin credential is never relayed to a provider.
- **Validation.** Every array `id` must be non-blank; every array key must be at least 32 characters. Ids and key values must both be unique across all three sets (`tokens_env`/`tokens_file`, `write_keys`, `read_keys`); a collision names the colliding ids and never logs a key value. A legacy `tokens_env` token shorter than 32 characters warns rather than failing, because those tokens predate the rule. `[server.admin]` still fails closed when all three sources are empty, but an array-only deployment (with `tokens_env` unset) boots.
- **No literals.** An array key written literally in the config file is **rejected at load**: it must be supplied by `${VAR}`, `${file:/abs/path}`, or a `SHUNT_*` environment override. This is stricter than shunt's other secret-typed fields, which only warn — see [`config-secrets.md`](config-secrets.md).

## Enforcement

Enforcement applies to `POST /v1/messages` when `[server.spend]` is configured. It reads the same state the Admin API writes.

**Principal.** A request is attributed to one principal, matched verbatim against a `user` cap's `user_id`: the static `[server.auth]` token's name, the verified inbound JWT's email, or the gateway-login email. A request whose chain injects a gateway-held credential but carries no identity shares the reserved principal `shunt:anonymous`. A chain made only of passthrough routes (the caller's own upstream credential) is neither enforced nor metered, since the caller pays; the same holds for a chain made only of `type = "noop"` entries, which never reach an upstream.

The three identity sources share one key space. A static token name, a JWT identity claim and a gateway-login email that are equal share one counter and one `user` cap, so keep them distinct. The id `shunt:anonymous` is reserved for requests that carry no identity.

**Effective cap.** Per period (`daily`, `weekly`, `monthly`) the cap is the principal's own `user` row, else the `organization` row, else unlimited. The organization row is a per-seat default, not a shared pool: every principal gets its own counter against it. A `user` row with `amount: null` is an explicit unlimited and wins over the organization row.

**Pre-check only.** The check runs before any upstream-capable step, with no reservation. Concurrent in-flight requests are all admitted against the same counters, so a principal can overshoot a cap by whatever is in flight when it is reached.

**Windows** are UTC calendar windows: daily from 00:00 UTC, weekly from Monday 00:00 UTC, monthly from the 1st 00:00 UTC.

**Refusal.** A principal at or over any cap gets `429` with an Anthropic-shape `billing_error`:

```
spend limit reached (monthly; resets 2026-11-01 00:00 UTC)
```

When `blocked_message` is set it follows after an em dash (`... UTC) — Request an increase from FinOps.`). The response carries `retry-after` (seconds to the reset, at least 1) and `x-should-retry: false`, and names the reached cap that resets last; when two caps reset at the same instant (daily and monthly on a month's last day) the longer period is named. `POST /v1/messages/count_tokens` is never refused and never metered.

**`fail_closed_on_error`.** The meter is "unavailable" for a principal when one of that principal's persisted counter records failed to restore (see [Counter persistence](#counter-persistence)); the flag lifts when the windows that record could cover have elapsed. The default (`false`) forwards the request and logs a warning. With `true` a principal that has a cap (in any period) is refused with `429 billing_error` `spend limit unavailable` (plus the `blocked_message` suffix), no `retry-after`, and `anthropic-ratelimit-unified-overage-disabled-reason: fetch_error`; a principal with no cap has nothing to enforce and is forwarded.

## Metering

The meter records a charge for each billed response, on the daily, weekly and monthly counters of the principal at once.

- **Coverage.** Streamed and non-streamed responses, translated adapters, and committed streams. The charge is priced on the **upstream** model that served the turn, through the pricing table above. Server-side web search is added per request.
- **Per route.** Metering is decided per serving route: a call served by a passthrough route is not metered, even inside a chain that also holds an injecting route. Enforcement is decided per request.
- **Only 2xx is billed.** An error response is not generated output.
- **Aborted streams.** The streamed `usage` is read from the upstream's cumulative counts. When the final output count never arrives (a client disconnect, an upstream cut, or an adapter-synthesized end after a cut), output is billed at a floor of one token per 4 characters of text delivered to the client, rounded up. Input is billed from the `message_start` value; for translated Responses routes that is the local prompt estimate under the default `count_tokens = "tiktoken"`, and 0 for providers set to `count_tokens = "estimate"`.
- **Unreadable non-streamed bodies.** A non-streamed body larger than 4 MiB, cut, or without a readable `usage` is billed a byte floor of one output token per 4 bytes. A readable `usage` that omits `output_tokens` keeps its other counts, and its output is billed at one token per 4 characters of the message's generated content, the same floor as an aborted stream.
- **Side calls.** Router judge, classifier and escalation calls, and gated turns, are metered exactly once against the requesting principal. A gated streamed capture that `gated_max_duration_ms` or `gated_max_bytes` cuts is billed for what it received (including the chunk that crossed the byte cap). An Anthropic request pinned to the provider's `classifier_model` is priced on that model; a translated reply marked truncated bills at least the delivered-text floor.

Known unmetered cases: a router judge reply that is truncated, times out, or exceeds its size bound, a non-streamed gated capture that was cut, and a non-streamed `2xx` body dropped before its first byte was polled (the client disconnected after the upstream answered).

Limit: counters are u64 femto-USD and saturate at about $18,446.74 per principal per window, so a cap of 1,844,675 cents or more can never be reached (a follow-up issue tracks widening).

## Rate-limit headers

For a principal with a cap in any period, shunt strips every upstream `anthropic-ratelimit-*` header from the response (on every status of an admitted `/v1/messages` request) and, on a `2xx`, writes the principal's own binding-cap view in their place. Claude Code 2.1.225 and later reads these to show its usage warnings. The binding cap is the exceeded one if any, else the highest utilization (a tie goes to the longer period).

| Header | Meaning |
| :-- | :-- |
| `anthropic-ratelimit-unified-status` | `allowed`, `allowed_warning` (above 75%) or, on the refusal, `rejected` |
| `anthropic-ratelimit-unified-reset`, `...-overage-reset` | Unix seconds of the binding cap's reset |
| `anthropic-ratelimit-unified-overage-utilization` | Utilization of the binding cap, rounded to two decimals and capped at 0.99 while below the cap; can exceed 1 once the cap is exceeded |
| `anthropic-ratelimit-unified-overage-surpassed-threshold` | `0.75`, `0.95` or `1`, only once the utilization is strictly above that threshold |
| `anthropic-ratelimit-unified-representative-claim`, `...-overage-status` | `overage` and the status again; 2xx only |
| `anthropic-ratelimit-unified-overage-period` | Refusal only: the exceeded period |
| `anthropic-ratelimit-unified-overage-disabled-reason` | `org_spend_cap_reached` on the over-cap refusal, `fetch_error` on the fail-closed refusal |

The over-cap `429` carries the "exceeded" set without `representative-claim` or `overage-status`, so Claude Code shows the refusal's own message. A principal with no cap, an unmetered request, a fail-open forward, and `POST /v1/messages/count_tokens` (never assessed) carry no shunt-written headers; for an uncapped principal, and for `count_tokens`, the upstream's headers pass through untouched.

## Counter persistence

Counters live in a sibling of the caps file, `<state_path stem>.counters.json` (default `$HOME/.shunt/gateway-spend.counters.json`), with its own versioned envelope (`{"version": 1, "counters": [...]}`). The caps file is never rewritten by a counter flush, so rolling back to a build that only knows stage-1 caps still boots.

- Changed counters are flushed at most every 10 seconds, and once more at shutdown after the listener drains, bounded by a fixed 5 seconds, independent of `[server] shutdown_timeout_seconds` (worst-case shutdown extends by at most that much; see `bounded-shutdown.md`).
- `spend_retention_months` prunes windows that started before the first day of the month that many calendar months back and have already ended; a window that is still open is never pruned. The prune runs on every tick of the 10-second loop, including an idle one, so a lowered (hot-reloaded) value applies at the next tick without a new charge. With `state_path = ""` (or no resolvable home) the same tick still prunes the in-memory counters and writes no file, so a memory-only process does not grow its counters for its lifetime.
- Spend from responses still open when the shutdown drain deadline expires is not persisted.
- A counter record this build cannot read is carried through unchanged, except that shunt may add a reserved `carried_until` field (below); when it still names its principal, that principal is unavailable (see `fail_closed_on_error`) until the windows the record could cover have elapsed. A carried record with no usable start (missing, unparseable, or implausibly far in the future) is dropped once that deadline passes. When such a record is a JSON object, shunt stores the deadline inside it as a reserved `"carried_until"` field (unix seconds) at the first flush after load, even when no charge has arrived, and a later restart reuses that stored deadline instead of computing a fresh one from the new clock, so a restart does not flag the principal again. A record that is not a JSON object cannot carry the field and is dropped at the first write after load. Every other record loads normally.
- An unreadable envelope or an unsupported version aborts startup, like the caps file.
- `state_path = ""` keeps the counters in memory only.

## Admin API

The following routes exist only when `[server.spend]` is configured at startup, independently of `[server.gateway]`:

- `GET /v1/organizations/spend_limits`
- `POST /v1/organizations/spend_limits`
- `GET /v1/organizations/spend_limits/{id}`
- `DELETE /v1/organizations/spend_limits/{id}`
- `GET /v1/organizations/spend_limits/effective`

Send the `[server.admin]` credential in the configured admin header (`x-shunt-admin-token` by default) or in `x-api-key`; both slots are accepted. A write credential — a `write_keys` entry or a legacy `tokens_env`/`tokens_file` pair — can use every operation. A read credential (`read_keys`) can use `GET` and receives `403` on mutations. An invalid or missing credential receives `401`.

`POST` accepts `{scope, amount, period}`. `scope` supports `{ "type": "organization" }` and `{ "type": "user", "user_id": "..." }`; `user_id` must contain 1–256 bytes. `period` is `daily`, `weekly`, or `monthly`; when the client omits it, shunt uses `monthly`. `amount` is a whole-number string of USD cents in the inclusive range `0`–`9999999999999999999`, or `null`; shunt strips leading zeroes before storing and returning it. The empty string, non-ASCII digits, and values outside this range receive `400 invalid_request_error`. Canonical `"0"` is distinct from unlimited. A supplied `currency` must equal `USD`.

The operation upserts by `(scope, period)`. Replacing a cap keeps its original `id` and `created_at`; submitting the same numeric amount again, including a representation with leading zeroes, is idempotent and preserves `updated_at` without adding an audit record or rewriting the state file. Each actual mutation appends an audit record containing the before and after snapshots and the actor — `admin-key:<id>` for a `write_keys` entry, `admin-token:<name>` for a legacy `tokens_env`/`tokens_file` pair — then persists caps and audit records in one JSON write. Until the configured retention sweep is implemented, shunt keeps the newest 10,000 audit records across both records understood by the running version and opaque carry-through records, dropping the oldest records from their merged persisted order when a mutation exceeds the cap. Audit ids remain monotonic across this trimming.

List results use `{data, has_more, first_id, last_id}`. `limit` defaults to 20 and accepts 1–1000. `after_id` and `before_id` are mutually exclusive. Results remain in creation order; `has_more` describes additional results in the selected traversal direction.

Every response includes `request-id`. Error bodies use:

```json
{
  "type": "error",
  "error": { "type": "invalid_request_error", "message": "..." },
  "request_id": "req_..."
}
```

### GET effective

`GET /v1/organizations/spend_limits/effective` returns one row per principal and period: the cap that binds it and its period-to-date spend. A read or write credential works.

Query parameters:

| Parameter | Meaning |
| :-- | :-- |
| `limit` | Principals per page, 1–1000, default 20 (counts principals, not rows: each principal contributes one row per requested period) |
| `period[]` | Repeatable: `daily`, `weekly`, `monthly`. Default: all three |
| `user_ids[]` | Repeatable, at most 100. Returns exactly those principals (unpaginated, `next_page` null), whether or not they have spent; `q` still filters them |
| `q` | Case-insensitive substring filter on the principal, at most 256 characters |
| `sort` | `spend_desc`; requires exactly one `period[]` |
| `page` | The opaque `next_page` token from the previous response |

Without `user_ids[]` the rows cover every principal with a counter in a retained window. Each row has `scope` (`{"type": "user", "user_id": ...}`), `groups` (always `[]`), `actor`, `amount` and `source`/`spend_limit_id` (the binding row, or null when unlimited), `currency`, `period`, and `period_to_date_spend` in US cents with up to three decimals. `actor` is derived from the principal alone: a static `[server.auth]` token name fills `name`, a principal that looks like an email (exactly one `@`, a non-empty local part, and a domain containing a `.`) fills `email_address`, and anything else, such as an opaque JWT `sub`, and `shunt:anonymous` have both null. Invalid parameters return `400 invalid_request_error`: `limit: must be between 1 and 1000`, `period[]: must be one of daily, weekly, monthly`, `user_ids[]: at most 100 entries per request`, `q: too long`, `sort: must be spend_desc`, `sort=spend_desc requires exactly one period[]`, `page: invalid page token`.

## Not yet implemented

- `GET /v1/organizations/spend_limits/audit`
- Retention sweeps of audit records and identity data (`audit_retention_days`, `identity_retention_days`)
- `rbac_group`, `seat_tier` and `organization_service` scopes, and `group_limit_mode`
- The dollar figure Claude Code requests from `GET /api/oauth/usage`
- The inbound Codex endpoint (`[server.codex_endpoint]`): it is neither enforced nor metered, so a principal reaching shunt only through it spends uncapped. Tracked in [#733](https://github.com/pleaseai/shunt/issues/733)
