# Gateway spend limits: stage 1

This stage adds an authenticated Admin API for storing spend caps. It does not apply the caps to inference traffic yet.

## Configuration

Enable the API under the existing gateway block. Store keys in environment variables as comma-separated `id:key` pairs; each key must contain at least 32 characters. Key ids and key values must each be unique across both variables. If either uniqueness check fails, configuration validation reports the colliding ids without logging the key value. If both variables are unset or blank, shunt starts with the routes enabled but logs both variable names and warns that every request will receive `401`.

```toml
[server.gateway.admin]
write_keys_env = "SHUNT_GATEWAY_ADMIN_WRITE_KEYS"
read_keys_env = "SHUNT_GATEWAY_ADMIN_READ_KEYS"
blocked_message = "Request an increase from FinOps."
audit_retention_days = 365
spend_retention_months = 13
identity_retention_days = 90
group_limit_mode = "min"
state_path = "~/.shunt/gateway-spend.json"

[server.gateway.enforcement]
fail_closed_on_error = false
```

`state_path = ""` keeps caps and audit records in memory only. When shunt cannot resolve a home directory, the default path also becomes memory-only. The state file uses a versioned JSON envelope and an atomic private-file replacement. At restore, shunt drops each cap whose amount, currency, object type, or user id fails the same validation as `POST`, and logs a warning containing that cap's id and invalid field; other caps in the file still load. The path is fixed at boot; configuration reloads do not move the process-lifetime store to a different file.

The retention settings, `blocked_message`, `group_limit_mode`, and `fail_closed_on_error` are parsed now for configuration compatibility. Stage 1 does not run a retention sweep, resolve group limits, customize an enforcement error, or perform enforcement. `fail_closed_on_error = true` requires `[server.gateway.admin]`, even though enforcement is deferred.

This configuration deliberately uses environment-variable indirection instead of inline key values. shunt does not expand `${VAR}` placeholders in TOML, and its other authentication surfaces use the same `*_env` convention.

## Admin API

The following routes exist only when `[server.gateway.admin]` is configured at startup:

- `GET /v1/organizations/spend_limits`
- `POST /v1/organizations/spend_limits`
- `GET /v1/organizations/spend_limits/{id}`
- `DELETE /v1/organizations/spend_limits/{id}`

Send `x-api-key` with a write key for all operations. A read key can use `GET` and receives `403` on mutations. An invalid or missing key receives `401`.

`POST` accepts `{scope, amount, period}`. `scope` supports `{ "type": "organization" }` and `{ "type": "user", "user_id": "..." }`; `user_id` must contain 1–256 bytes. `period` is `daily`, `weekly`, or `monthly`; when the client omits it, shunt uses `monthly`. `amount` is a whole-number string of 1–19 USD-cent digits or `null`; `"0"` is distinct from unlimited. A supplied `currency` must equal `USD`.

The operation upserts by `(scope, period)`. Replacing a cap keeps its original `id` and `created_at`. Each mutation appends an audit record containing the before and after snapshots and the actor `admin-key:<id>`, then persists caps and audit records in one JSON write.

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
