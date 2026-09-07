# Gateway spend limits: stage 1

This stage adds an authenticated Admin API for storing spend caps. It does not apply the caps to inference traffic yet.

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

The retention settings, `blocked_message`, `group_limit_mode`, and `fail_closed_on_error` are parsed now for configuration compatibility. Stage 1 does not run a retention sweep, resolve group limits, customize an enforcement error, or perform enforcement.

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

`multiplier` scales every resolved rate, list price and override alike, and models a discount: it must be a finite number of at least `0.000001` and at most `1`. Each override row supplies all four rates in USD per million tokens, and each must be finite and at least `0.001`. Both floors exist because rates are stored as whole femto-USD (1e-15 USD) per token: their product is exactly 1 femto-USD per token, the smallest nonzero rate the meter can carry, so anything below either floor quantizes to `0` and prices requests at nothing while looking like a valid discount. A missing rate is a parse error; an out-of-range rate, an out-of-range multiplier, a blank `upstream`, an `upstream` that names no configured upstream (the error lists the ones that exist), and two rows pricing the same model on one upstream all fail configuration validation at boot. Two rows collide when they name the same model, not merely the same string: `claude-sonnet-4-6` and `claude-sonnet-4-6-20260217` are one model on one upstream. A row whose `model` is neither a built-in nor a model that any `[[models]]`, `[[routes]]`, or `[[route_prefixes]]` entry can request **on that row's own `upstream`** is a warning rather than an error — the row is kept, but nothing will ever match it. The check is per upstream and covers prefix routes: a model mapped only as another upstream's `upstream_model` does not make the row usable, while a model served by a `[[route_prefixes]]` entry on the row's own upstream does. A configuration with none of those three tables forwards the client's model string as-is, so no row is warned about there.

Model ids are normalized before matching: Claude Code's `[1m]` context-window hint, a Bedrock region prefix and `anthropic.` namespace (including hyphenated regions such as `us-gov.`), a Bedrock `-v<major>:<minor>` version suffix, and a dated snapshot suffix in either the Anthropic (`-20260217`) or Vertex (`@20251101`) form are all stripped. The OpenRouter and Vercel AI Gateway form is normalized too: the `anthropic/` namespace is stripped and the dotted version is hyphenated, so `anthropic/claude-opus-4.8` prices as `claude-opus-4-8`. OpenRouter's floating aliases (`~anthropic/claude-sonnet-latest`) name no version and stay unpriceable.

Rates are matched most-specific-first, for one upstream at a time:

1. an override row whose `model` equals the **upstream** model id (case-insensitively);
2. otherwise an override row whose `model` equals the **client** model id;
3. otherwise an override row naming the same built-in model by a different id — a dated snapshot (`claude-opus-4-5@20251101`), a Bedrock id (`us.anthropic.claude-sonnet-4-6-20260217-v1:0`) — matched against the upstream model first, then the client model;
4. otherwise the built-in list price of the **upstream** model — never of the client model, so a built-in client id remapped to a non-Anthropic upstream model is not billed at Anthropic rates;
5. otherwise nothing: the request cannot be priced, and the meter decides what that means.

The built-in list-price catalog is the fallback, in USD per million tokens, and covers the Claude models shunt routes to. Server-side web search is priced per request ($0.01) rather than per token, and override rows never change it; only the multiplier applies.

All amounts are USD **estimates**. They are computed from the token counts an upstream reports against published list prices, not read back from a provider invoice, so they will not reconcile to the cent with a bill.

The pricing table and its resolver are implemented and validated at boot, but nothing calls them yet: the spend meter that will price requests is not implemented (see [Not yet implemented](#not-yet-implemented)).

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

## Admin API

The following routes exist only when `[server.spend]` is configured at startup, independently of `[server.gateway]`:

- `GET /v1/organizations/spend_limits`
- `POST /v1/organizations/spend_limits`
- `GET /v1/organizations/spend_limits/{id}`
- `DELETE /v1/organizations/spend_limits/{id}`

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

## Not yet implemented

- Spend enforcement on `/v1/messages`, including `429 billing_error`
- Token usage metering — the pricing table and rate resolver (`[server.spend.pricing]`) are in place and validated at boot, but no meter reads token usage or prices a request yet, so nothing calls them
- `GET /v1/organizations/spend_limits/effective`
- `GET /v1/organizations/spend_limits/audit`
- Hourly retention sweeps
- `rbac_group`, `seat_tier`, and `organization_service` scopes
