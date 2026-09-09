//! Tool-schema sanitization for Responses-protocol upstreams.
//!
//! The OpenAI backend validates every function tool's `parameters` against
//! the JSON Schema meta-schema with format checking on, and the meta-schema
//! declares `pattern` (and each `patternProperties` key) as `format: regex`.
//! That check compiles the string with Python's `re`, which rejects several
//! constructs ECMAScript accepts — most visibly Unicode property escapes
//! (`\p{Cc}`), JavaScript named groups (`(?<name>…)`), brace code points
//! (`\u{…}`) and letter escapes it does not define (`\z`, `\h`). A schema
//! carrying one fails the *whole request*:
//!
//! ```text
//! 400 Invalid schema for function 'Artifact':
//!   '^(?!__.*__$)[^\p{Cc}\p{Cf}\p{Zl}\p{Zp}"\\./[\]]{1,200}$' is not a 'regex'.
//! ```
//!
//! The error text is the Python `jsonschema` library's format-check message,
//! which is what pins the validator down. Claude Code's tool schemas are
//! written for a JavaScript client and the Anthropic API accepts them as-is,
//! so the offending patterns are dropped here rather than forwarded. Outside
//! strict mode a `pattern` is advisory — the model reads it, nothing enforces
//! it — so a dropped one costs a hint, not a capability; the `description`
//! usually restates the constraint anyway. Patterns Python accepts (lookahead
//! included) are kept.

use serde_json::Value;

/// Remove every `pattern` keyword, and every `patternProperties` entry, whose
/// regex Python's `re` would refuse to compile.
///
/// Walks schema positions only: `properties`, `$defs` and the other keyed maps
/// hold schemas under names the *tool* chose, so a property called `pattern`
/// is neither a keyword nor a regex; `default`, `enum`, `const` and the
/// example keywords hold instances and are left verbatim.
pub(crate) fn strip_unsupported_patterns(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map
                .get("pattern")
                .and_then(Value::as_str)
                .is_some_and(|pattern| !python_re_accepts(pattern))
            {
                map.remove("pattern");
            }
            if let Some(Value::Object(entries)) = map.get_mut("patternProperties") {
                entries.retain(|key, _| python_re_accepts(key));
            }
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "properties" | "$defs" | "definitions" | "patternProperties"
                    | "dependentSchemas" => {
                        if let Value::Object(schemas) = child {
                            schemas.values_mut().for_each(strip_unsupported_patterns);
                        }
                    }
                    "dependencies" => {
                        if let Value::Object(entries) = child {
                            entries
                                .values_mut()
                                .filter(|entry| entry.is_object())
                                .for_each(strip_unsupported_patterns);
                        }
                    }
                    "dependentRequired" | "default" | "example" | "examples" | "enum" | "const" => {
                    }
                    _ => strip_unsupported_patterns(child),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_unsupported_patterns),
        _ => {}
    }
}

/// Whether Python's `re.compile` accepts `pattern`.
///
/// This is not a regex parser; it flags the escapes and group openers that
/// `sre_parse` rejects and JavaScript-authored schemas actually use, and
/// accepts everything else. An escaped ASCII letter must be one Python
/// defines (`\d \D \s \S \w \W \b \B \A \Z` and the C escapes `\a \f \n \r
/// \t \v`), or `\x`/`\u`/`\U` followed by exactly 2/4/8 hex digits, or a
/// `\N{name}`; `(?<` must open a lookbehind (`(?<=`, `(?<!`), never a named
/// group (Python spells that `(?P<`).
fn python_re_accepts(pattern: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                let Some(&escaped) = chars.get(i + 1) else {
                    // A trailing backslash: "bad escape (end of pattern)".
                    return false;
                };
                if !escape_accepted(escaped, &chars[i + 2..]) {
                    return false;
                }
                i += 2;
            }
            '(' if chars.get(i + 1) == Some(&'?') && chars.get(i + 2) == Some(&'<') => {
                if !matches!(chars.get(i + 3), Some('=') | Some('!')) {
                    return false;
                }
                i += 3;
            }
            _ => i += 1,
        }
    }
    true
}

fn escape_accepted(escaped: char, rest: &[char]) -> bool {
    let hex_run =
        |count: usize| rest.len() >= count && rest[..count].iter().all(|c| c.is_ascii_hexdigit());
    match escaped {
        'x' => hex_run(2),
        'u' => hex_run(4),
        'U' => hex_run(8),
        'N' => rest.first() == Some(&'{'),
        'a' | 'f' | 'n' | 'r' | 't' | 'v' | 'b' | 'B' | 'd' | 'D' | 's' | 'S' | 'w' | 'W' | 'A'
        | 'Z' => true,
        c if c.is_ascii_alphabetic() => false,
        // Digits (group references, octal) and punctuation escape to
        // themselves or a group; Python accepts all of them syntactically.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The `field` pattern of Claude Code's `Artifact` tool: the regex the
    /// OpenAI backend rejected (Unicode property escapes inside a class).
    const ARTIFACT_FIELD: &str = r#"^(?!__.*__$)[^\p{Cc}\p{Cf}\p{Zl}\p{Zp}"\\./[\]]{1,200}$"#;
    /// Its sibling `collection` pattern: a lookahead, which Python accepts.
    const ARTIFACT_COLLECTION: &str = r"^(?!\.\.?(?:\/|$))[A-Za-z0-9_\-.~:@+]{1,200}(?:\/(?!\.\.?(?:\/|$))[A-Za-z0-9_\-.~:@+]{1,200}){0,14}$";

    #[test]
    fn python_re_rejects_javascript_only_constructs() {
        for pattern in [
            ARTIFACT_FIELD,
            r"^\p{L}+$",
            r"\P{Cc}",
            r"(?<name>a)",
            r"\u{1F600}",
            r"a\z",
            r"\h",
            r"\x4",
            r"trailing\",
        ] {
            assert!(!python_re_accepts(pattern), "should reject {pattern:?}");
        }
    }

    #[test]
    fn python_re_accepts_what_it_compiles() {
        for pattern in [
            ARTIFACT_COLLECTION,
            r"^[a-z]+$",
            r"(?<=a)b",
            r"(?<!a)b",
            r"(?P<name>a)",
            r"^[\]a]+$",
            r"\d\w\s\b\B\A\Z\n\t",
            r"\u0041\x41\U00000041\N{BULLET}",
            r"\.\-\/\1",
            "",
        ] {
            assert!(python_re_accepts(pattern), "should accept {pattern:?}");
        }
    }

    #[test]
    fn strips_rejected_patterns_at_every_schema_position_and_keeps_the_rest() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "field": {"type": "string", "pattern": ARTIFACT_FIELD},
                "collection": {"type": "string", "pattern": ARTIFACT_COLLECTION},
                "writes": {"type": "array", "items": {"type": "string", "pattern": r"\p{L}"}},
                "either": {"anyOf": [{"pattern": r"\P{N}"}, {"pattern": "^ok$"}]},
                "keyed": {"patternProperties": {r"^\p{L}": {}, "^[a-z]+$": {}}}
            }
        });
        strip_unsupported_patterns(&mut schema);
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "field": {"type": "string"},
                    "collection": {"type": "string", "pattern": ARTIFACT_COLLECTION},
                    "writes": {"type": "array", "items": {"type": "string"}},
                    "either": {"anyOf": [{}, {"pattern": "^ok$"}]},
                    "keyed": {"patternProperties": {"^[a-z]+$": {}}}
                }
            })
        );
    }

    #[test]
    fn instances_and_tool_chosen_names_are_left_alone() {
        let mut schema = json!({
            "properties": {
                "pattern": {"type": "string", "default": {"pattern": r"\p{L}"}},
                "choice": {"enum": [{"pattern": r"\p{L}"}], "const": {"pattern": r"\p{L}"}}
            }
        });
        let expected = schema.clone();
        strip_unsupported_patterns(&mut schema);
        assert_eq!(schema, expected);
    }
}
