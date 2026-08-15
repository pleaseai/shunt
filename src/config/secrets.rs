//! `${VAR}` / `${file:/abs/path}` reference substitution over a parsed config
//! file tree, and the [`Secret`] newtype that redacts a resolved value in
//! `Debug`/`Serialize`.
//!
//! Substitution runs once, directly on the file's `raw` text, between
//! `Config::load` reading the file and handing it to figment (`Config::load`
//! in `../config.rs`): the text is parsed into a generic `toml::Value` /
//! `serde_yaml::Value` tree, every string leaf is scanned for references,
//! and the tree is re-serialized back to text for figment to parse as
//! before. It applies to every string value in the file tree (not keys),
//! including nested tables and array elements, and is not recursive: a
//! resolved value is never re-scanned for further references. It only ever
//! touches the file layer — `SHUNT_*` env overrides are never passed through
//! this pass.
//!
//! Supported forms in a string:
//! - `${VAR}` — replaced with the value of environment variable `VAR`. May
//!   be embedded inside a longer string (`"Bearer ${TOKEN}"`). An undefined
//!   variable is a load error.
//! - `${file:/abs/path}` — replaced with the trimmed contents of the file at
//!   an absolute path. Must be the field's entire value; a `${file:...}`
//!   embedded in a longer string is a load error rather than being silently
//!   left as-is.
//! - `$${` — an escape for a literal `${`, so a free-text field can contain
//!   that sequence. The `{` it emits never opens a reference, and scanning
//!   continues after it, so a later `${VAR}` in the same string still
//!   resolves.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{ConfigError, ConfigFormat};

/// A string config value that must never be echoed back — a token, DSN, or
/// header value. `Debug` and `Serialize` both render `[redacted]`.
///
/// INVARIANT: because `Serialize` is lossy (it unconditionally emits
/// `[redacted]`, real value or not), a `Secret` must never be reachable when
/// `Config::load` re-serializes a Rust value and re-extracts it through
/// figment — doing so would silently replace the real value with the
/// literal string `[redacted]`, which then decodes right back into a
/// `Secret` on the next extract as if `[redacted]` were the operator's
/// actual DSN or token. An empty *default* would not save this: the danger
/// is reachability, not the default's emptiness, since any concrete value a
/// user ever configures is what gets clobbered. The two round-trips in
/// `../config.rs` stay safe for different reasons:
/// - `Figment::from(Serialized::defaults(Self::default()))` at
///   `../config.rs:2361` seeds the whole `Config`, including every
///   `Secret`-bearing section (`SentryConfig`, `OtelConfig`,
///   `GatewayTelemetryDestination`) — but each of those sections sits behind
///   an `Option` that defaults to `None` with `#[serde(skip_serializing_if =
///   "Option::is_none")]`, so `Serialized::defaults` never serializes into
///   the section at all, and no `Secret` inside it is ever visited.
/// - `Figment::from(Serialized::default("providers", &self.providers))` at
///   `../config.rs:2606` is a different figment API (singular `default`,
///   namespaced under one key) that only round-trips the `providers` map,
///   which has no `Secret` field at all.
///
/// Keep it that way: every `Secret`-bearing section must stay behind an
/// `Option` defaulting to `None` with `skip_serializing_if =
/// "Option::is_none"` (never hoisted out so it is unconditionally present in
/// `Config::default()`), and a `Secret` must never be placed under
/// `providers`. Concretely, hoisting a section like `SentryConfig` out from
/// behind its `Option` would make `Serialized::defaults` serialize it (with
/// `dsn` rendering as `[redacted]`) and figment re-extract that back as
/// `dsn`'s real value — and since `SentryConfig::enabled()` is
/// `!dsn.expose().trim().is_empty()`, a non-empty `[redacted]` string would
/// silently enable Sentry reporting with a garbage DSN.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

const REDACTED: &str = "[redacted]";

impl Secret {
    /// Explicit, deliberately-named accessor: every call site that reads the
    /// real value should read as an intentional unwrap of the redaction.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

impl Serialize for Secret {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(REDACTED)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        record_literal_hit(&value);
        Ok(Self(value))
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl PartialEq<str> for Secret {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Secret {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// Load-scoped map from a literal (non-reference) string value found in the
/// config file to the dotted field path(s) it appeared at, plus the set of
/// paths a `Secret::deserialize` call has actually matched against it.
struct LiteralContext {
    map: HashMap<String, Vec<String>>,
    /// Values that must never trigger the literal-secret warning: every
    /// value produced by resolving a `${VAR}`/`${file:...}` reference
    /// anywhere in the file, plus the current value of every `SHUNT_*`
    /// process env var. Checked before consulting `map`, so a `Secret` fed
    /// by a reference or an env override never warns even if some unrelated
    /// literal elsewhere happens to share the same string.
    never_literal: HashSet<String>,
    hits: BTreeSet<String>,
    /// Count of literal secret occurrences that could not be attributed to
    /// exactly one Secret-shaped path in `map` so far — either because the
    /// value matched more than one such path (an unresolvable ambiguity) or
    /// because it matched none (see `is_secret_field_path`'s allowlist-drift
    /// note). Never guessed, and never dropped silently.
    unattributed: usize,
}

thread_local! {
    static LOAD_CONTEXT: RefCell<Option<LiteralContext>> = const { RefCell::new(None) };
}

/// RAII guard scoping a single `Config::load` call's literal-value -> path
/// map so `Secret::deserialize` can (best-effort) record which config-file
/// paths held a secret written literally, for the aggregated boot warning in
/// `Config::load`. Dropping the guard clears the thread-local, so a config
/// load without one active (e.g. a unit test deserializing a struct
/// directly) degrades safely to "no warning" rather than panicking or
/// leaking state across loads.
pub(crate) struct LiteralScope;

impl LiteralScope {
    pub(crate) fn enter(map: HashMap<String, Vec<String>>, never_literal: HashSet<String>) -> Self {
        LOAD_CONTEXT.with(|context| {
            *context.borrow_mut() = Some(LiteralContext {
                map,
                never_literal,
                hits: BTreeSet::new(),
                unattributed: 0,
            });
        });
        Self
    }

    /// Field paths recorded as holding a literal secret so far, sorted for a
    /// deterministic warning message. Safe to call with no active scope
    /// (returns empty).
    pub(crate) fn hits() -> Vec<String> {
        LOAD_CONTEXT.with(|context| {
            context
                .borrow()
                .as_ref()
                .map(|context| context.hits.iter().cloned().collect())
                .unwrap_or_default()
        })
    }

    /// Count of literal secret occurrences that could not be attributed to a
    /// single field path so far — the value appeared at more than one
    /// Secret-shaped path, or at none at all. See `LiteralContext::
    /// unattributed` for both causes. Safe to call with no active scope
    /// (returns 0).
    pub(crate) fn unattributed_count() -> usize {
        LOAD_CONTEXT.with(|context| {
            context
                .borrow()
                .as_ref()
                .map(|context| context.unattributed)
                .unwrap_or(0)
        })
    }
}

impl Drop for LiteralScope {
    fn drop(&mut self) {
        LOAD_CONTEXT.with(|context| *context.borrow_mut() = None);
    }
}

/// Formats a dotted field path for the aggregated literal-secret warning,
/// matching the `[section].leaf` style `Config::load`'s warning message uses
/// (e.g. `sentry.dsn` -> `[sentry].dsn`, `otel.headers.authorization` ->
/// `[otel.headers].authorization`).
pub(crate) fn format_literal_path(path: &str) -> String {
    match path.rsplit_once('.') {
        Some((prefix, leaf)) => format!("[{prefix}].{leaf}"),
        None => path.to_string(),
    }
}

/// Closed-form allowlist of the dotted field paths that are actually
/// `Secret` fields in the config schema: `sentry.dsn`,
/// `otel.headers.<key>`,
/// `server.gateway.telemetry.forward_to.<index>.headers.<key>`,
/// `server.gateway.session.jwt_secret` (scalar or, per array element,
/// `server.gateway.session.jwt_secret.<index>`), and the admin key arrays
/// (`is_admin_key_path`). Used to
/// filter candidate paths for a literal value down to Secret-shaped ones
/// before deciding whether the attribution is unambiguous, so a plain
/// (non-`Secret`) field's path is never named in the warning — even when it
/// happens to share a literal value with a real `Secret` field.
///
/// This list exists only to make attribution *precise* — it is not, and
/// must never become, the definition of which fields are secret (that is
/// what the `Secret` type itself is for; issue #345 deliberately rejected a
/// separate designated-fields list). It will drift: the day someone adds a
/// new `Secret` field without adding its path shape here, `record_literal_hit`
/// degrades to counting that field's literal value as unattributed rather
/// than naming it — it must never go silent. Add the new path shape here
/// when adding a `Secret` field, to keep the warning message precise.
fn is_secret_field_path(path: &str) -> bool {
    if path == "sentry.dsn" {
        return true;
    }
    if is_admin_key_path(path) {
        return true;
    }
    let segments: Vec<&str> = path.split('.').collect();
    if segments.len() >= 3 && segments[0] == "otel" && segments[1] == "headers" {
        return true;
    }
    if segments.len() >= 7
        && segments[0] == "server"
        && segments[1] == "gateway"
        && segments[2] == "telemetry"
        && segments[3] == "forward_to"
        && segments[5] == "headers"
    {
        return true;
    }
    // `server.gateway.session.jwt_secret` (bare-string form) or
    // `server.gateway.session.jwt_secret.<index>` (array element).
    if (segments.len() == 4 || segments.len() == 5)
        && segments[0] == "server"
        && segments[1] == "gateway"
        && segments[2] == "session"
        && segments[3] == "jwt_secret"
    {
        return true;
    }
    false
}

/// `server.admin.write_keys.<index>.key` or
/// `server.admin.read_keys.<index>.key` — the two array element paths whose
/// literals `Config::load` rejects outright instead of warning about.
fn is_admin_key_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    segments.len() == 5
        && segments[0] == "server"
        && segments[1] == "admin"
        && matches!(segments[2], "write_keys" | "read_keys")
        && segments[4] == "key"
}

/// Reject an admin key written literally in the config file. Unlike the
/// pre-existing `Secret` fields — `sentry.dsn`, `otel.headers.*`, the gateway
/// telemetry headers, and `server.gateway.session.jwt_secret`, which existing
/// deployments hold literals in and which therefore only warn — the key arrays
/// are new and have no such users, so a literal there costs nothing to refuse.
///
/// This cannot live in `Secret::deserialize`, which sees only the value and
/// never the path. `literals` is exactly the set of values written directly
/// into the file: a `${VAR}`/`${file:}` reference resolves into
/// `Substituted::resolved_values` instead, and a `SHUNT_*` override never
/// passes through the file layer at all. The offending paths are sorted so a
/// config with several literal keys always names the same one.
pub(crate) fn reject_literal_admin_keys(
    literals: &HashMap<String, Vec<String>>,
) -> Result<(), ConfigError> {
    let mut offending: Vec<&str> = literals
        .values()
        .flatten()
        .filter(|path| is_admin_key_path(path))
        .map(String::as_str)
        .collect();
    offending.sort_unstable();
    match offending.first() {
        Some(path) => Err(ConfigError::LiteralAdminKey {
            path: (*path).to_string(),
        }),
        None => Ok(()),
    }
}

/// Called from `Secret::deserialize`. Records the sole config-file path that
/// `value` was written to literally as a warning-worthy hit, when that
/// attribution is unambiguous. A no-op with no active scope.
///
/// An empty or whitespace-only value never warns — it is the documented
/// sentinel for "disabled" (e.g. `SHUNT_SENTRY__DSN=""`), never a
/// credential. A value already known to come from resolving a
/// `${VAR}`/`${file:...}` reference, or to match a current `SHUNT_*` env
/// var, never warns either, even if some unrelated literal elsewhere shares
/// the same string. Otherwise, candidate paths for `value` are narrowed to
/// Secret-shaped ones (`is_secret_field_path`) before checking for
/// ambiguity: exactly one candidate is attributed by path; zero or more
/// than one is counted as unattributed rather than guessed or dropped
/// silently — see `is_secret_field_path` for why zero candidates can still
/// happen for a genuine `Secret` field.
fn record_literal_hit(value: &str) {
    if value.trim().is_empty() {
        return;
    }
    LOAD_CONTEXT.with(|context| {
        let mut context = context.borrow_mut();
        let Some(context) = context.as_mut() else {
            return;
        };
        if context.never_literal.contains(value) {
            return;
        }
        let Some(paths) = context.map.get(value) else {
            return;
        };
        let mut candidates = paths.iter().filter(|path| is_secret_field_path(path));
        let Some(first) = candidates.next() else {
            // No path in the map matched `is_secret_field_path` — either a
            // genuine attribution ambiguity resolved to zero after
            // filtering, or (more likely) the allowlist is missing a shape
            // for a `Secret` field added since it was last updated. Either
            // way this is still a literal secret that must not vanish
            // silently: degrade to an unattributed count rather than
            // suppressing the warning outright.
            context.unattributed += 1;
            return;
        };
        if candidates.next().is_some() {
            context.unattributed += 1;
        } else {
            context.hits.insert(first.clone());
        }
    });
}

/// The current value of every `SHUNT_`-prefixed process environment
/// variable. Seeds `LiteralContext::never_literal` so a `Secret` field
/// supplied by a `SHUNT_*` env override never triggers the literal-secret
/// warning, even if its value happens to also appear literally elsewhere in
/// the config file.
///
/// Deliberately `vars_os`, not `vars`: the latter panics when *any* variable
/// in the environment is not valid Unicode, including one wholly unrelated
/// to shunt, because the conversion happens before a caller can filter by
/// prefix. That would turn an advisory warning's bookkeeping into a process
/// panic on every load, check, and hot reload. Undecodable entries are
/// dropped instead — losing nothing, since a non-Unicode value can never
/// equal one of the `String` values this set is compared against.
pub(crate) fn shunt_env_values() -> HashSet<String> {
    std::env::vars_os()
        .filter(|(key, _)| key.to_str().is_some_and(|key| key.starts_with("SHUNT_")))
        .filter_map(|(_, value)| value.into_string().ok())
        .collect()
}

/// A parsed-and-substituted config file, ready to hand to figment.
pub(crate) struct Substituted {
    /// The file, re-serialized in its original format with every reference
    /// resolved.
    pub text: String,
    /// Every literal (non-reference) string value found in the file, mapped
    /// to the dotted path(s) it appeared at.
    pub literals: HashMap<String, Vec<String>>,
    /// Every value produced by resolving a `${VAR}`/`${file:...}` reference
    /// anywhere in the file — never a literal-secret warning candidate,
    /// however it later shows up in a `Secret` field's parsed value.
    pub resolved_values: HashSet<String>,
}

/// Parses `raw` in `format`, substitutes every `${...}` reference found in a
/// string leaf, and re-serializes the result back to text in the same
/// format for figment to parse as before.
pub(crate) fn substitute(raw: &str, format: ConfigFormat) -> Result<Substituted, ConfigError> {
    match format {
        ConfigFormat::Toml => substitute_toml(raw),
        ConfigFormat::Yaml => substitute_yaml(raw),
    }
}

fn substitute_toml(raw: &str) -> Result<Substituted, ConfigError> {
    let mut value: toml::Value =
        raw.parse()
            .map_err(|error: toml::de::Error| ConfigError::InvalidConfigSyntax {
                message: error.to_string(),
            })?;
    let mut literals = HashMap::new();
    let mut resolved_values = HashSet::new();
    walk_toml(
        &mut value,
        String::new(),
        &mut literals,
        &mut resolved_values,
    )?;
    let text = toml::to_string(&value).map_err(|error| ConfigError::InvalidConfigSyntax {
        message: error.to_string(),
    })?;
    Ok(Substituted {
        text,
        literals,
        resolved_values,
    })
}

fn substitute_yaml(raw: &str) -> Result<Substituted, ConfigError> {
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(raw).map_err(|error| ConfigError::InvalidConfigSyntax {
            message: error.to_string(),
        })?;
    let mut literals = HashMap::new();
    let mut resolved_values = HashSet::new();
    walk_yaml(
        &mut value,
        String::new(),
        &mut literals,
        &mut resolved_values,
    )?;
    let text = serde_yaml::to_string(&value).map_err(|error| ConfigError::InvalidConfigSyntax {
        message: error.to_string(),
    })?;
    Ok(Substituted {
        text,
        literals,
        resolved_values,
    })
}

fn child_path(parent: &str, segment: &str) -> String {
    if parent.is_empty() {
        segment.to_string()
    } else {
        format!("{parent}.{segment}")
    }
}

fn walk_toml(
    value: &mut toml::Value,
    path: String,
    literals: &mut HashMap<String, Vec<String>>,
    resolved_values: &mut HashSet<String>,
) -> Result<(), ConfigError> {
    match value {
        toml::Value::String(s) => {
            let resolved = resolve_string(s, &path)?;
            if resolved.is_literal {
                literals
                    .entry(resolved.value.clone())
                    .or_default()
                    .push(path);
            } else {
                resolved_values.insert(resolved.value.clone());
            }
            *s = resolved.value;
        }
        toml::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                walk_toml(
                    item,
                    child_path(&path, &index.to_string()),
                    literals,
                    resolved_values,
                )?;
            }
        }
        toml::Value::Table(map) => {
            for (key, item) in map.iter_mut() {
                let child = child_path(&path, key);
                walk_toml(item, child, literals, resolved_values)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn walk_yaml(
    value: &mut serde_yaml::Value,
    path: String,
    literals: &mut HashMap<String, Vec<String>>,
    resolved_values: &mut HashSet<String>,
) -> Result<(), ConfigError> {
    match value {
        serde_yaml::Value::String(s) => {
            let resolved = resolve_string(s, &path)?;
            if resolved.is_literal {
                literals
                    .entry(resolved.value.clone())
                    .or_default()
                    .push(path);
            } else {
                resolved_values.insert(resolved.value.clone());
            }
            *s = resolved.value;
        }
        serde_yaml::Value::Sequence(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                walk_yaml(
                    item,
                    child_path(&path, &index.to_string()),
                    literals,
                    resolved_values,
                )?;
            }
        }
        serde_yaml::Value::Mapping(map) => {
            for (key, item) in map.iter_mut() {
                let child = child_path(&path, &yaml_key_to_string(key));
                walk_yaml(item, child, literals, resolved_values)?;
            }
        }
        serde_yaml::Value::Tagged(tagged) => {
            walk_yaml(&mut tagged.value, path, literals, resolved_values)?;
        }
        _ => {}
    }
    Ok(())
}

fn yaml_key_to_string(key: &serde_yaml::Value) -> String {
    match key {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        _ => "?".to_string(),
    }
}

struct Resolved {
    value: String,
    /// True when no `${VAR}`/`${file:...}` reference was actually resolved
    /// (an untouched string, or one that only underwent `$${` -> `${`
    /// unescaping — no secret was consulted either way).
    is_literal: bool,
}

/// Scans `input` for `${...}` references and escapes, resolving each one in
/// place. `path` is the dotted field path, used only for error messages.
fn resolve_string(input: &str, path: &str) -> Result<Resolved, ConfigError> {
    let mut out = String::with_capacity(input.len());
    let mut resolved_any = false;
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // `$${` escapes to a literal `${`: both characters are emitted as
        // ordinary text and the `{` never opens a reference. Scanning
        // continues normally after it, so a later `${VAR}` in the same string
        // still resolves.
        if bytes[i] == b'$' && i + 2 < bytes.len() && bytes[i + 1] == b'$' && bytes[i + 2] == b'{' {
            out.push_str("${");
            i += 3;
            continue;
        }
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let content_start = i + 2;
            let Some(rel_end) = input[content_start..].find('}') else {
                return Err(ConfigError::UnterminatedReference {
                    path: path.to_string(),
                });
            };
            let content_end = content_start + rel_end;
            let content = &input[content_start..content_end];
            let after_close = content_end + 1;
            if content.is_empty() {
                return Err(ConfigError::EmptyReferenceName {
                    path: path.to_string(),
                });
            }
            if let Some(colon) = content.find(':') {
                let scheme = &content[..colon];
                let file_path = &content[colon + 1..];
                if scheme != "file" {
                    return Err(ConfigError::UnknownReferenceScheme {
                        path: path.to_string(),
                        scheme: scheme.to_string(),
                    });
                }
                // A file reference must be the field's entire value: nothing
                // resolved before it in this string, and nothing left after it.
                let is_whole_value = out.is_empty() && after_close == input.len();
                if !is_whole_value {
                    return Err(ConfigError::EmbeddedFileReference {
                        path: path.to_string(),
                    });
                }
                // `starts_with('/')` would reject every Windows absolute path
                // (`C:\secrets\token`), making `${file:...}` unusable there;
                // `Path::is_absolute` is the portable check for both platforms.
                if !std::path::Path::new(file_path).is_absolute() {
                    return Err(ConfigError::RelativeFileReference {
                        path: path.to_string(),
                        file: file_path.to_string(),
                    });
                }
                let contents = std::fs::read_to_string(file_path).map_err(|error| {
                    ConfigError::UnreadableReferenceFile {
                        path: path.to_string(),
                        file: file_path.to_string(),
                        message: error.to_string(),
                    }
                })?;
                return Ok(Resolved {
                    value: contents.trim().to_string(),
                    is_literal: false,
                });
            }
            if !is_valid_var_name(content) {
                return Err(ConfigError::InvalidReferenceVarName {
                    path: path.to_string(),
                    name: content.to_string(),
                });
            }
            let value = std::env::var(content).map_err(|error| match error {
                std::env::VarError::NotPresent => ConfigError::UndefinedReferenceVar {
                    path: path.to_string(),
                    var: content.to_string(),
                },
                std::env::VarError::NotUnicode(_) => ConfigError::NonUnicodeReferenceVar {
                    path: path.to_string(),
                    var: content.to_string(),
                },
            })?;
            out.push_str(&value);
            resolved_any = true;
            i = after_close;
            continue;
        }
        let ch = input[i..].chars().next().expect("i is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(Resolved {
        value: out,
        is_literal: !resolved_any,
    })
}

fn is_valid_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(input: &str) -> Result<String, ConfigError> {
        resolve_string(input, "test.field").map(|resolved| resolved.value)
    }

    #[test]
    fn var_reference_resolves() {
        std::env::set_var("SHUNT_SECRETS_TEST_VAR_A", "hello");
        assert_eq!(resolve("${SHUNT_SECRETS_TEST_VAR_A}").unwrap(), "hello");
        std::env::remove_var("SHUNT_SECRETS_TEST_VAR_A");
    }

    #[test]
    fn var_reference_resolves_embedded_and_multiple() {
        std::env::set_var("SHUNT_SECRETS_TEST_VAR_B", "tok");
        std::env::set_var("SHUNT_SECRETS_TEST_VAR_C", "en");
        assert_eq!(
            resolve("Bearer ${SHUNT_SECRETS_TEST_VAR_B}${SHUNT_SECRETS_TEST_VAR_C}!").unwrap(),
            "Bearer token!"
        );
        std::env::remove_var("SHUNT_SECRETS_TEST_VAR_B");
        std::env::remove_var("SHUNT_SECRETS_TEST_VAR_C");
    }

    #[test]
    fn undefined_var_reference_names_path_and_var() {
        std::env::remove_var("SHUNT_SECRETS_TEST_VAR_UNDEFINED");
        let error = resolve("${SHUNT_SECRETS_TEST_VAR_UNDEFINED}").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("test.field"), "{message}");
        assert!(
            message.contains("SHUNT_SECRETS_TEST_VAR_UNDEFINED"),
            "{message}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn non_unicode_var_reference_is_distinguished_from_undefined() {
        // `std::env::var` returns `Err` both when a variable is unset and
        // when it is set to bytes that are not valid Unicode; these must not
        // be reported with the same "which is not set" message.
        use std::os::unix::ffi::OsStringExt;
        let invalid = std::ffi::OsString::from_vec(vec![0xFF, 0xFE, 0xFD]);
        std::env::set_var("SHUNT_SECRETS_TEST_VAR_NON_UNICODE", invalid);
        let error = resolve("${SHUNT_SECRETS_TEST_VAR_NON_UNICODE}").unwrap_err();
        assert!(matches!(error, ConfigError::NonUnicodeReferenceVar { .. }));
        let message = error.to_string();
        assert!(message.contains("test.field"), "{message}");
        assert!(
            message.contains("SHUNT_SECRETS_TEST_VAR_NON_UNICODE"),
            "{message}"
        );
        std::env::remove_var("SHUNT_SECRETS_TEST_VAR_NON_UNICODE");
    }

    #[test]
    #[cfg(unix)]
    fn shunt_env_values_survives_non_unicode_variables() {
        // `std::env::vars` panics when *any* variable in the environment is
        // not valid Unicode -- including one that has nothing to do with
        // shunt, because the conversion happens before a caller can filter
        // by prefix. Since this scan runs on every load, check, and hot
        // reload, that would turn an unrelated env var into a process panic.
        // Seed both shapes and assert the scan still returns.
        use std::os::unix::ffi::OsStringExt;
        let invalid = || std::ffi::OsString::from_vec(vec![0xFF, 0xFE, 0xFD]);
        // Unrelated key, undecodable value: must not even be looked at.
        std::env::set_var("NOT_SHUNT_SECRETS_TEST_NON_UNICODE", invalid());
        // `SHUNT_`-prefixed key, undecodable value: dropped, not fatal. It
        // can never equal one of the `String`s this set is compared against.
        std::env::set_var("SHUNT_SECRETS_TEST_ENV_SCAN_NON_UNICODE", invalid());
        // A value unique enough that it cannot collide with a literal in a
        // concurrently-running config test's fixture.
        std::env::set_var(
            "SHUNT_SECRETS_TEST_ENV_SCAN",
            "shunt-secrets-test-env-scan-sentinel",
        );

        let values = shunt_env_values();

        assert!(
            values.contains("shunt-secrets-test-env-scan-sentinel"),
            "decodable SHUNT_ values must still be collected: {values:?}"
        );
        std::env::remove_var("NOT_SHUNT_SECRETS_TEST_NON_UNICODE");
        std::env::remove_var("SHUNT_SECRETS_TEST_ENV_SCAN_NON_UNICODE");
        std::env::remove_var("SHUNT_SECRETS_TEST_ENV_SCAN");
    }

    #[test]
    fn file_reference_resolves_trimmed() {
        let dir = std::env::temp_dir().join(format!(
            "shunt-secrets-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.txt");
        std::fs::write(&path, "  top-secret\n\n").unwrap();
        let reference = format!("${{file:{}}}", path.display());
        assert_eq!(resolve(&reference).unwrap(), "top-secret");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_reference_names_path_not_contents() {
        let path = std::env::temp_dir().join(format!(
            "shunt-secrets-test-missing-{}-{}",
            std::process::id(),
            line!()
        ));
        let reference = format!("${{file:{}}}", path.display());
        let error = resolve(&reference).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("test.field"), "{message}");
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[test]
    fn relative_file_reference_is_an_error() {
        let error = resolve("${file:relative/path}").unwrap_err();
        assert!(matches!(error, ConfigError::RelativeFileReference { .. }));
    }

    #[cfg(windows)]
    #[test]
    fn windows_absolute_file_reference_is_not_rejected_as_relative() {
        // `C:\...` does not start with `/`, so the old `starts_with('/')`
        // check rejected every Windows absolute path as relative. The file
        // below doesn't exist, so resolution still fails, but with
        // `UnreadableReferenceFile` rather than `RelativeFileReference` —
        // proving the path was accepted as absolute before the read failed.
        let error = resolve(r"${file:C:\definitely\does\not\exist\token}").unwrap_err();
        assert!(matches!(error, ConfigError::UnreadableReferenceFile { .. }));
    }

    #[test]
    fn embedded_file_reference_is_an_error() {
        let error = resolve("prefix ${file:/abs/path}").unwrap_err();
        assert!(matches!(error, ConfigError::EmbeddedFileReference { .. }));
        let error = resolve("${file:/abs/path} suffix").unwrap_err();
        assert!(matches!(error, ConfigError::EmbeddedFileReference { .. }));
        let error = resolve("${file:/abs/path}${OTHER}").unwrap_err();
        assert!(matches!(error, ConfigError::EmbeddedFileReference { .. }));
    }

    #[test]
    fn escape_yields_literal_and_is_not_resolved() {
        // The variable is set so this is non-vacuous: it proves the escape
        // suppressed a resolution that would otherwise have happened, rather
        // than just echoing a string that had nothing to resolve.
        std::env::set_var("SHUNT_SECRETS_TEST_ESCAPED", "resolved");
        assert_eq!(
            resolve("$${SHUNT_SECRETS_TEST_ESCAPED}").unwrap(),
            "${SHUNT_SECRETS_TEST_ESCAPED}"
        );
        // The escaped output is emitted as text, never re-scanned, so a
        // single `$` before `{` still opens a real reference.
        assert_eq!(
            resolve("${SHUNT_SECRETS_TEST_ESCAPED}").unwrap(),
            "resolved"
        );
        // `$$` is only special immediately before `{`.
        assert_eq!(resolve("$$").unwrap(), "$$");
        assert_eq!(resolve("$$foo").unwrap(), "$$foo");
        // An escape earlier in the string does not disarm a later reference.
        assert_eq!(
            resolve("$${literal} ${SHUNT_SECRETS_TEST_ESCAPED}").unwrap(),
            "${literal} resolved"
        );
        // Unescaping alone consults no secret, so the value stays literal for
        // the boot warning; a real reference in the same string does not.
        assert!(
            resolve_string("$${literal}", "test.field")
                .unwrap()
                .is_literal
        );
        assert!(
            !resolve_string("$${literal} ${SHUNT_SECRETS_TEST_ESCAPED}", "test.field")
                .unwrap()
                .is_literal
        );
        // The whole-value rule for file references still counts text an
        // escape produced, so this is embedded rather than whole-value.
        assert!(matches!(
            resolve("$${x} ${file:/etc/hostname}"),
            Err(ConfigError::EmbeddedFileReference { .. })
        ));
        std::env::remove_var("SHUNT_SECRETS_TEST_ESCAPED");
    }

    #[test]
    fn malformed_forms_are_errors() {
        assert!(matches!(
            resolve("${unterminated"),
            Err(ConfigError::UnterminatedReference { .. })
        ));
        assert!(matches!(
            resolve("${}"),
            Err(ConfigError::EmptyReferenceName { .. })
        ));
        assert!(matches!(
            resolve("${not a var name}"),
            Err(ConfigError::InvalidReferenceVarName { .. })
        ));
        assert!(matches!(
            resolve("${foo:bar}"),
            Err(ConfigError::UnknownReferenceScheme { .. })
        ));
    }

    #[test]
    fn substitution_reaches_nested_tables_and_arrays() {
        std::env::set_var("SHUNT_SECRETS_TEST_VAR_NESTED", "nested-value");
        let raw = "[a]\nb = [\"x\", \"${SHUNT_SECRETS_TEST_VAR_NESTED}\"]\n[[a.c]]\nd = \"${SHUNT_SECRETS_TEST_VAR_NESTED}\"\n";
        let substituted = substitute(raw, ConfigFormat::Toml).unwrap();
        assert!(substituted.text.contains("nested-value"));
        assert!(!substituted.text.contains("SHUNT_SECRETS_TEST_VAR_NESTED"));
        std::env::remove_var("SHUNT_SECRETS_TEST_VAR_NESTED");
    }

    #[test]
    fn secret_debug_and_serialize_never_show_the_real_value() {
        #[derive(Debug, Serialize)]
        struct Holder {
            token: Secret,
        }
        let holder = Holder {
            token: Secret::from("super-secret-value"),
        };
        let debug = format!("{holder:?}");
        assert!(!debug.contains("super-secret-value"), "{debug}");
        assert!(debug.contains("[redacted]"), "{debug}");

        let json = serde_json::to_string(&holder).unwrap();
        assert!(!json.contains("super-secret-value"), "{json}");
        assert!(json.contains("[redacted]"), "{json}");
    }

    #[test]
    fn format_literal_path_renders_the_section_leaf_style() {
        assert_eq!(format_literal_path("sentry.dsn"), "[sentry].dsn");
        assert_eq!(
            format_literal_path("otel.headers.authorization"),
            "[otel.headers].authorization"
        );
        // No-dot passthrough: a top-level path has no section to bracket.
        assert_eq!(format_literal_path("dsn"), "dsn");
    }

    #[test]
    fn literal_hit_at_a_path_missing_from_the_secret_field_allowlist_is_unattributed_not_silent() {
        // `is_secret_field_path` is a hand-maintained allowlist that will
        // drift the day someone adds a new `Secret` field without adding its
        // path shape here. This pins the fail-safe for that drift: a value
        // whose only candidate path fails `is_secret_field_path` must still
        // surface as an unattributed literal-secret hit, never vanish.
        let mut map = HashMap::new();
        map.insert(
            "drifted-secret-value".to_string(),
            vec!["some.future.secret_field".to_string()],
        );
        let _scope = LiteralScope::enter(map, HashSet::new());
        record_literal_hit("drifted-secret-value");
        assert_eq!(LiteralScope::unattributed_count(), 1);
        assert!(LiteralScope::hits().is_empty());
    }

    #[test]
    fn is_secret_field_path_matches_gateway_session_jwt_secret_scalar_and_array_forms() {
        // Pins the two path shapes `is_secret_field_path` must recognize for
        // `[server.gateway.session] jwt_secret`: the bare-string form and an
        // array element. If either arm were removed, `record_literal_hit`
        // would degrade both values to unattributed (see the drift test
        // above) instead of attributing them by path.
        let mut map = HashMap::new();
        map.insert(
            "scalar-session-secret".to_string(),
            vec!["server.gateway.session.jwt_secret".to_string()],
        );
        map.insert(
            "rotated-session-secret".to_string(),
            vec!["server.gateway.session.jwt_secret.1".to_string()],
        );
        let _scope = LiteralScope::enter(map, HashSet::new());
        record_literal_hit("scalar-session-secret");
        record_literal_hit("rotated-session-secret");
        assert_eq!(
            LiteralScope::hits(),
            vec![
                "server.gateway.session.jwt_secret".to_string(),
                "server.gateway.session.jwt_secret.1".to_string(),
            ]
        );
        assert_eq!(LiteralScope::unattributed_count(), 0);
    }
}
