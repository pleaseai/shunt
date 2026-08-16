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
- Token usage metering and model pricing
- `GET /v1/organizations/spend_limits/effective`
- `GET /v1/organizations/spend_limits/audit`
- Hourly retention sweeps
- `rbac_group`, `seat_tier`, and `organization_service` scopes
