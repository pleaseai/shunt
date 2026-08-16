# Config secret references (`${VAR}` / `${file:}`)

## 0. Problem

Config files often need a value that shouldn't live in the file itself — a
Sentry DSN, an OTLP collector token, a header forwarded to a telemetry
backend. Until now the only way to keep such a value out of the file was the
handful of purpose-built `*_env` fields (`tokens_env`, `jwt_secret_env`,
`client_secret_env`, `api_key_env`, `users_env`, `token_env`) and
`tokens_file`. Fields that don't have an `*_env` counterpart had no indirect
form at all. This adds a generic substitution pass so any string value in the
config file can be supplied indirectly, and adds redaction for the fields
most likely to hold a secret literal.

## 1. Syntax

Before the config file is parsed, shunt scans every string value for two
reference forms:

- **`${VAR}`** — replaced with the value of environment variable `VAR`. It
  may be embedded inside a longer string, e.g. `"Bearer ${TOKEN}"`. Config
  load fails if `VAR` is undefined.
- **`${file:/abs/path}`** — replaced with the contents of the file at
  `/abs/path`, trimmed. The path must be absolute, and the reference must be
  the field's **entire** value — it cannot be embedded in a longer string.
  Config load fails if the file is unreadable, the path is relative, or the
  reference is embedded in a longer string.

`$${` escapes to a literal `${`, so a field can still contain that character
sequence as ordinary text.

Resolution is **not recursive** — a value produced by `${VAR}` or
`${file:...}` is not re-scanned for further references.

This pass works identically for TOML and YAML config files, and applies only
to the config file. Values coming from `SHUNT_*` environment overrides are
used as-is; they are not substituted.

## 2. Coexistence with `*_env` fields

`tokens_env`, `client_secret_env`, `api_key_env`, `users_env`, `token_env`,
and `tokens_file` keep their existing meaning unchanged — each still names an
environment variable (or, for `tokens_file`, a file path) that shunt reads at
request time. They are not deprecated. `${VAR}` / `${file:...}` substitution
is a separate, additive idiom: it inlines a value directly into the config
file instead of pointing at an external name. Both are supported, and like
any other config-file string a `*_env` field's own value also passes through
substitution before shunt reads the environment variable it names.

`jwt_secret_env` is the one exception: it is deprecated (still fully
supported) in favor of [`[server.gateway.session] jwt_secret`](../site/src/content/docs/reference/configuration.md#servergatewaysession-optional),
which is itself a `${VAR}` / `${file:...}`-capable field — the same
indirection this document describes, applied directly to the secret-typed
field instead of through a separate `*_env` name. See
[gateway-login.md](gateway-login.md) for the deprecation and conflict rules.

## 3. Redaction

Six field paths are typed as a redacting secret and render as `[redacted]` in
diagnostic output (logs, `shunt check`, admin/debug surfaces): `[sentry]
dsn`, each value in `[otel] headers`, each `headers` value under
`[server.gateway.telemetry] forward_to[]`, `[server.gateway.session]
jwt_secret`, and the `key` of each `[[server.admin.write_keys]]` and
`[[server.admin.read_keys]]` entry.

For the first four, writing the value literally in the config file still works
exactly as before — that is a redaction change, not a validation change. On
boot, if such a field holds a literal (rather than a `${VAR}` /
`${file:...}` reference), shunt logs one advisory warning naming the affected
field paths — never the values — suggesting `${VAR}` / `${file:...}` instead.
The warning is advisory only; it does not fail config load or `shunt check`.

**The two admin key arrays are the exception: a literal there is refused, not
warned about.** A `key` written directly in the config file fails config load
with `[server.admin.write_keys.<index>.key] holds an admin key written
literally in the config file` (likewise for `read_keys`); the value itself is
never echoed. It must be supplied by `${VAR}`, `${file:/abs/path}`, or a
`SHUNT_*` environment override. The four older paths only warn because
deployments already hold literals in them and tightening the rule would refuse
to start a config that works today; the key arrays are new, so no deployment
holds a literal there yet and refusing costs nothing. An admin key is also a
higher-value secret than the others — it can provision upstream accounts and
administer spend limits — so keeping it out of the file is worth a hard failure.

## 4. Interaction with hot reload

The substitution pass reruns on every config load, including boot, `shunt
check`, and both [hot-reload](config-reload.md) triggers (SIGHUP and the file
watcher). A `${file:...}`-backed secret can therefore be rotated by
overwriting the referenced file's contents, without restarting shunt —
trigger a reload afterward (SIGHUP, or a write to the config file itself) to
pick up the new value, since the file watcher only watches the config file's
own parent directory, not arbitrary `${file:...}` target paths.

Re-resolution is not the same as taking effect. The reloaded config always
holds the new value, but whether the running gateway uses it follows that
field's own reload behavior, and two of the secret-typed field groups are
restart-only: `[sentry]` (the Sentry client is initialized once at
startup) and `[otel]` (the OpenTelemetry exporters likewise). Rotating a
`${file:...}` secret in either section updates the config and logs
`requires a restart to apply`, while the running client keeps the old
credential until shunt restarts. `[server.gateway.telemetry].forward_to`
headers are read from the live config per request, and the `[server.admin]`
key arrays are re-resolved into the admin-auth state on every reload, so those
do rotate on reload. See `warn_on_restart_only_changes` in `src/reload.rs` for
the full restart-only set.

Reload's usual fail-safe behavior applies: if the reloaded config fails to
resolve a reference (undefined variable, unreadable file), the reload is
rejected and the currently-running config stays live. See
[config-reload.md](config-reload.md) for the full reload contract, including
which fields require a restart regardless of how their value was supplied.
