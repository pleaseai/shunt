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

`tokens_env`, `jwt_secret_env`, `client_secret_env`, `api_key_env`,
`users_env`, `token_env`, and `tokens_file` keep their existing meaning
unchanged — each still names an environment variable (or, for `tokens_file`,
a file path) that shunt reads at request time. They are not deprecated.
`${VAR}` / `${file:...}` substitution is a separate, additive idiom: it
inlines a value directly into the config file instead of pointing at an
external name. Both are supported, and like any other config-file string a
`*_env` field's own value also passes through substitution before shunt reads
the environment variable it names.

## 3. Redaction

Three fields are typed as a redacting secret and render as `[redacted]` in
diagnostic output (logs, `shunt check`, admin/debug surfaces): `[sentry]
dsn`, each value in `[otel] headers`, and each `headers` value under
`[server.gateway.telemetry] forward_to[]`.

Writing one of these literally in the config file still works exactly as
before — this is a redaction change, not a validation change. On boot, if a
secret-typed field holds a literal (rather than a `${VAR}` / `${file:...}`
reference), shunt logs one advisory warning naming the affected field paths
— never the values — suggesting `${VAR}` / `${file:...}` instead. The
warning is advisory only; it does not fail config load or `shunt check`.

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
field's own reload behavior, and two of the three secret-typed field groups
are restart-only: `[sentry]` (the Sentry client is initialized once at
startup) and `[otel]` (the OpenTelemetry exporters likewise). Rotating a
`${file:...}` secret in either section updates the config and logs
`requires a restart to apply`, while the running client keeps the old
credential until shunt restarts. `[server.gateway.telemetry].forward_to`
headers are read from the live config per request, so those do rotate on
reload. See `warn_on_restart_only_changes` in `src/reload.rs` for the full
restart-only set.

Reload's usual fail-safe behavior applies: if the reloaded config fails to
resolve a reference (undefined variable, unreadable file), the reload is
rejected and the currently-running config stays live. See
[config-reload.md](config-reload.md) for the full reload contract, including
which fields require a restart regardless of how their value was supplied.
