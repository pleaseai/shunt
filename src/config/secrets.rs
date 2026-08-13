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
use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use super::{ConfigError, ConfigFormat};

/// A string config value that must never be echoed back — a token, DSN, or
/// header value. `Debug` and `Serialize` both render `[redacted]`.
///
/// INVARIANT: because `Serialize` is lossy (it does not round-trip the real
/// value), a `Secret` must never be re-serialized and then re-deserialized
/// through figment's `Serialized::defaults` layer — doing so would silently
/// replace the real value with the literal string `[redacted]`. `Config::load`
/// only feeds `Serialized::defaults` with `Config::default()` (no `Secret`
/// field defaults to a non-empty value) and with the normalized `providers`
/// map (which has no `Secret` field at all). Keep it that way: never give a
/// `Secret` field a non-empty default, and never place a `Secret` under
/// `providers`.
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
    hits: BTreeSet<String>,
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
    pub(crate) fn enter(map: HashMap<String, Vec<String>>) -> Self {
        LOAD_CONTEXT.with(|context| {
            *context.borrow_mut() = Some(LiteralContext {
                map,
                hits: BTreeSet::new(),
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

/// Called from `Secret::deserialize`. Records `path` as a warning-worthy hit
/// if `value` is present in the active load's literal map (i.e. it was
/// written verbatim in the config file rather than resolved from a
/// `${...}` reference). A no-op with no active scope.
fn record_literal_hit(value: &str) {
    LOAD_CONTEXT.with(|context| {
        if let Some(context) = context.borrow_mut().as_mut() {
            if let Some(paths) = context.map.get(value) {
                for path in paths {
                    context.hits.insert(path.clone());
                }
            }
        }
    });
}

/// A parsed-and-substituted config file, ready to hand to figment.
pub(crate) struct Substituted {
    /// The file, re-serialized in its original format with every reference
    /// resolved.
    pub text: String,
    /// Every literal (non-reference) string value found in the file, mapped
    /// to the dotted path(s) it appeared at.
    pub literals: HashMap<String, Vec<String>>,
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
    walk_toml(&mut value, String::new(), &mut literals)?;
    let text = toml::to_string(&value).map_err(|error| ConfigError::InvalidConfigSyntax {
        message: error.to_string(),
    })?;
    Ok(Substituted { text, literals })
}

fn substitute_yaml(raw: &str) -> Result<Substituted, ConfigError> {
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(raw).map_err(|error| ConfigError::InvalidConfigSyntax {
            message: error.to_string(),
        })?;
    let mut literals = HashMap::new();
    walk_yaml(&mut value, String::new(), &mut literals)?;
    let text = serde_yaml::to_string(&value).map_err(|error| ConfigError::InvalidConfigSyntax {
        message: error.to_string(),
    })?;
    Ok(Substituted { text, literals })
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
) -> Result<(), ConfigError> {
    match value {
        toml::Value::String(s) => {
            let resolved = resolve_string(s, &path)?;
            if resolved.is_literal {
                literals
                    .entry(resolved.value.clone())
                    .or_default()
                    .push(path);
            }
            *s = resolved.value;
        }
        toml::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                walk_toml(item, child_path(&path, &index.to_string()), literals)?;
            }
        }
        toml::Value::Table(map) => {
            for (key, item) in map.iter_mut() {
                let child = child_path(&path, key);
                walk_toml(item, child, literals)?;
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
) -> Result<(), ConfigError> {
    match value {
        serde_yaml::Value::String(s) => {
            let resolved = resolve_string(s, &path)?;
            if resolved.is_literal {
                literals
                    .entry(resolved.value.clone())
                    .or_default()
                    .push(path);
            }
            *s = resolved.value;
        }
        serde_yaml::Value::Sequence(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                walk_yaml(item, child_path(&path, &index.to_string()), literals)?;
            }
        }
        serde_yaml::Value::Mapping(map) => {
            for (key, item) in map.iter_mut() {
                let child = child_path(&path, &yaml_key_to_string(key));
                walk_yaml(item, child, literals)?;
            }
        }
        serde_yaml::Value::Tagged(tagged) => {
            walk_yaml(&mut tagged.value, path, literals)?;
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
                if !file_path.starts_with('/') {
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
}
