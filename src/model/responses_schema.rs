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
//! included) are kept, except `\N{…}`: Python reads it as a named character and
//! JavaScript as a literal `N`, so the two never agree on what it matches.

use serde_json::Value;

/// Python's repetition ceiling; `sre` rejects a bound at or above it.
const MAXREPEAT: u64 = 4_294_967_295;

/// Remove every `pattern` keyword, and every `patternProperties` entry, whose
/// regex Python's `re` would refuse to compile.
///
/// Walks the applicator keywords and nothing else, mirroring where the
/// validator itself looks: the meta-schema recognizes a subschema only under
/// those keywords, so a `pattern` sitting anywhere else is never format-checked
/// and dropping it would corrupt forwarded data to buy nothing. That covers a
/// property called `pattern` (`properties` and the other keyed maps hold
/// schemas under names the *tool* chose), the instance keywords `default`,
/// `enum` and `const`, and annotations or `x-` extensions, whose values are
/// arbitrary data that may legitimately contain a `pattern` field.
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
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    // Keys are regexes here, values are schemas: filter the
                    // keys, then recurse into what survived.
                    "patternProperties" => {
                        if let Value::Object(entries) = child {
                            entries.retain(|key, _| python_re_accepts(key));
                            entries.values_mut().for_each(strip_unsupported_patterns);
                        }
                    }
                    "properties" | "$defs" | "definitions" | "dependentSchemas" => {
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
                    // Applicators whose value is a schema, or an array of them
                    // — the `Value::Array` arm below walks the elements.
                    "items"
                    | "additionalItems"
                    | "prefixItems"
                    | "contains"
                    | "unevaluatedItems"
                    | "additionalProperties"
                    | "propertyNames"
                    | "unevaluatedProperties"
                    | "allOf"
                    | "anyOf"
                    | "oneOf"
                    | "not"
                    | "if"
                    | "then"
                    | "else"
                    | "contentSchema" => strip_unsupported_patterns(child),
                    // Everything else is data, not a schema position.
                    _ => {}
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_unsupported_patterns),
        _ => {}
    }
}

/// Whether Python's `re.compile` accepts `pattern`.
///
/// This is not a regex parser; it tracks just enough state — character-class
/// context, capture-group count, lookbehind nesting — to flag the constructs
/// `sre_parse` rejects and JavaScript-authored schemas actually use, and
/// accepts everything else. The two failure directions are not symmetric: a
/// pattern wrongly kept fails the *whole* upstream request, while one wrongly
/// dropped costs an advisory hint, so every judgement call here rejects.
///
/// Outside a character class an escaped ASCII letter must be one Python
/// defines (`\d \D \s \S \w \W \b \B \A \Z` and the C escapes `\a \f \n \r
/// \t \v`), or `\x`/`\u`/`\U` followed by exactly 2/4/8 hex digits, with `\U`
/// also decoding within `0x10FFFF`; `(?<` must open a lookbehind (`(?<=`,
/// `(?<!`), never a named group (Python spells that `(?P<`). Inside a class
/// the accepted set is narrower — `\A \Z \B` are `bad escape` there, as are
/// the non-octal digits `\8` and `\9` — and a range endpoint may not be a
/// category escape (`[\w-.]` is `bad character range`). A braced quantifier's
/// bounds must stay below `MAXREPEAT`.
fn python_re_accepts(pattern: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    // `Some(first)` while inside `[…]`, where `first` is the index a literal
    // `]` may still occupy (`[]]`, `[^]]`).
    let mut class_start: Option<usize> = None;
    // Whether the previous class member was a category escape (`\d`, `\w`, …),
    // which cannot be the endpoint of a range.
    let mut prev_class_category = false;
    let mut groups = 0usize;
    // Whether the previous token was a zero-width anchor, which Python refuses
    // to quantify ("nothing to repeat") and JavaScript happily reads as a
    // literal (`\A` is an identity escape there).
    let mut prev_anchor = false;
    // One entry per open group; `true` marks a lookbehind, whose body Python
    // requires to be fixed width.
    let mut open_groups: Vec<bool> = Vec::new();

    while i < chars.len() {
        let c = chars[i];
        if let Some(first) = class_start {
            match c {
                '\\' => {
                    let Some(&escaped) = chars.get(i + 1) else {
                        return false;
                    };
                    if !class_escape_accepted(escaped, &chars[i + 2..]) {
                        return false;
                    }
                    prev_class_category = matches!(escaped, 'd' | 'D' | 's' | 'S' | 'w' | 'W');
                    i += 2;
                }
                ']' if i > first => {
                    class_start = None;
                    prev_class_category = false;
                    i += 1;
                }
                '-' if i > first && chars.get(i + 1).is_some_and(|&next| next != ']') => {
                    let next_is_category = chars.get(i + 1) == Some(&'\\')
                        && chars
                            .get(i + 2)
                            .is_some_and(|&e| matches!(e, 'd' | 'D' | 's' | 'S' | 'w' | 'W'));
                    if prev_class_category || next_is_category {
                        return false;
                    }
                    prev_class_category = false;
                    i += 1;
                }
                _ => {
                    prev_class_category = false;
                    i += 1;
                }
            }
            continue;
        }
        match c {
            '\\' => {
                let Some(&escaped) = chars.get(i + 1) else {
                    // A trailing backslash: "bad escape (end of pattern)".
                    return false;
                };
                if let Some((reference, digits)) = group_reference(&chars[i + 1..]) {
                    // Python resolves `\1`…`\99` against the groups opened so
                    // far and fails to compile when there is no such group;
                    // JavaScript reads an unmatched `\8` as a literal `8`.
                    if reference > groups {
                        return false;
                    }
                    // `sre` cannot compute a group reference's width, so any
                    // backreference inside a lookbehind is "look-behind
                    // requires fixed-width pattern" — `(a+)(?<=\1)b` compiles
                    // under ECMAScript and not under Python.
                    if open_groups.contains(&true) {
                        return false;
                    }
                    prev_anchor = false;
                    i += 1 + digits;
                } else {
                    if !escape_accepted(escaped, &chars[i + 2..]) {
                        return false;
                    }
                    prev_anchor = matches!(escaped, 'A' | 'Z' | 'b' | 'B');
                    i += 2;
                }
            }
            '[' => {
                let first = if chars.get(i + 1) == Some(&'^') {
                    i + 2
                } else {
                    i + 1
                };
                class_start = Some(first);
                prev_class_category = false;
                prev_anchor = false;
                i = first;
            }
            '(' => {
                let mut width = 1;
                let mut capturing = true;
                let mut lookbehind = false;
                if chars.get(i + 1) == Some(&'?') {
                    capturing = false;
                    width = 3;
                    match chars.get(i + 2) {
                        Some('<') => match chars.get(i + 3) {
                            Some('=') | Some('!') => {
                                lookbehind = true;
                                width = 4;
                            }
                            // JavaScript's named group; Python spells it `(?P<`.
                            _ => return false,
                        },
                        // `(?P<name>…)` captures; `(?P=name)` back-references.
                        Some('P') => capturing = chars.get(i + 3) == Some(&'<'),
                        Some(_) => {}
                        None => return false,
                    }
                }
                if capturing {
                    groups += 1;
                }
                open_groups.push(lookbehind);
                prev_anchor = false;
                i += width;
            }
            ')' => {
                if open_groups.pop().is_none() {
                    // "unbalanced parenthesis".
                    return false;
                }
                prev_anchor = false;
                i += 1;
            }
            '|' | '*' | '+' | '?' | '{' if open_groups.contains(&true) => {
                // "look-behind requires fixed-width pattern"; JavaScript has
                // allowed a variable-width lookbehind since ES2018.
                return false;
            }
            '*' | '+' | '?' if prev_anchor => {
                // "nothing to repeat": Python cannot quantify a zero-width
                // anchor, while `\A*` is a valid `A*` in JavaScript.
                return false;
            }
            '{' => {
                if let Some(bounds) = braced_quantifier(&chars, i) {
                    if prev_anchor || bounds.iter().any(|bound| *bound >= MAXREPEAT) {
                        return false;
                    }
                }
                prev_anchor = false;
                i += 1;
            }
            '^' | '$' => {
                prev_anchor = true;
                i += 1;
            }
            _ => {
                prev_anchor = false;
                i += 1;
            }
        }
    }
    // An unterminated class or group is a compile error of its own.
    class_start.is_none() && open_groups.is_empty()
}

/// A `\1`…`\99` group reference at the start of `rest` (which begins at the
/// digit), as `(group, digit count)`. `None` when the escape is not a
/// reference: `\0…`, or three octal digits, which Python reads as a character.
fn group_reference(rest: &[char]) -> Option<(usize, usize)> {
    let digits: Vec<u32> = rest
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .filter_map(|c| c.to_digit(10))
        .collect();
    if *digits.first()? == 0 {
        return None;
    }
    if digits.len() >= 3 && digits[..3].iter().all(|digit| *digit < 8) {
        return None;
    }
    let count = digits.len().min(2);
    let group = digits[..count]
        .iter()
        .fold(0usize, |value, digit| value * 10 + *digit as usize);
    Some((group, count))
}

fn escape_accepted(escaped: char, rest: &[char]) -> bool {
    match escaped {
        'a' | 'f' | 'n' | 'r' | 't' | 'v' | 'b' | 'B' | 'd' | 'D' | 's' | 'S' | 'w' | 'W' | 'A'
        | 'Z' => true,
        _ => shared_escape_accepted(escaped, rest),
    }
}

/// The narrower set `sre_parse._class_escape` accepts: the anchors `\A`, `\Z`
/// and `\B` are `bad escape` inside a class, and `\b` is a backspace there.
fn class_escape_accepted(escaped: char, rest: &[char]) -> bool {
    match escaped {
        'a' | 'f' | 'n' | 'r' | 't' | 'v' | 'b' | 'd' | 'D' | 's' | 'S' | 'w' | 'W' => true,
        // Not octal, and a class has no group references for them to be, so
        // Python calls these `bad escape`; JavaScript reads them as digits.
        '8' | '9' => false,
        _ => shared_escape_accepted(escaped, rest),
    }
}

fn shared_escape_accepted(escaped: char, rest: &[char]) -> bool {
    let hex_run =
        |count: usize| rest.len() >= count && rest[..count].iter().all(|c| c.is_ascii_hexdigit());
    match escaped {
        'x' => hex_run(2),
        'u' => hex_run(4),
        // Python builds the character with `chr()`, so anything above
        // `0x10FFFF` is `bad escape`. Four hex digits cannot reach it.
        'U' => hex_run(8) && hex_value(&rest[..8]) <= 0x0010_FFFF,
        // `\N{name}` is a *named* character in Python and a plain identity
        // escape (a literal `N`) in JavaScript, so the two never agree on what
        // the pattern means, and validating the name would need the Unicode
        // name table. Reject it: the asymmetry above prefers a dropped hint.
        'N' => false,
        // A legacy octal escape runs to three digits and Python caps its value
        // at `0o377`; JavaScript reads `\400` as `\40` then `0`.
        c if c.is_digit(8) => octal_escape_in_range(c, rest),
        c if c.is_ascii_alphabetic() => false,
        // The remaining digits and punctuation escape to themselves.
        _ => true,
    }
}

fn hex_value(digits: &[char]) -> u32 {
    digits
        .iter()
        .fold(0u32, |value, c| value * 16 + c.to_digit(16).unwrap_or(0))
}

/// The bounds of the braced quantifier opening at `chars[open]`, or `None`
/// when the brace is not a quantifier at all — `a{foo}`, a trailing `a{`, `{}`
/// — which Python reads as a literal `{` and keeps.
fn braced_quantifier(chars: &[char], open: usize) -> Option<Vec<u64>> {
    let mut bounds: Vec<u64> = Vec::new();
    let mut current: Option<u64> = None;
    let mut seen_comma = false;
    let mut i = open + 1;
    while i < chars.len() {
        match chars[i] {
            c if c.is_ascii_digit() => {
                let digit = u64::from(c.to_digit(10).unwrap_or(0));
                let next = current
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(digit);
                // Pin past the ceiling so a long digit run cannot wrap back in.
                current = Some(next.min(MAXREPEAT + 1));
                i += 1;
            }
            ',' if !seen_comma => {
                seen_comma = true;
                bounds.extend(current.take());
                i += 1;
            }
            '}' => {
                bounds.extend(current.take());
                return (!bounds.is_empty()).then_some(bounds);
            }
            _ => return None,
        }
    }
    None
}

/// Whether the octal escape opening at `escaped` — up to three digits, the
/// other two taken from `rest` — is within Python's `0o377` ceiling.
fn octal_escape_in_range(escaped: char, rest: &[char]) -> bool {
    let mut value = escaped.to_digit(8).unwrap_or(0);
    for digit in rest.iter().take(2).map_while(|c| c.to_digit(8)) {
        value = value * 8 + digit;
    }
    value <= 0o377
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
            // A range endpoint cannot be a category escape: "bad character
            // range". JavaScript reads `[\w-.]` as a three-member class.
            r"[\w-.]",
            r"[a-\d]",
            // `\A`, `\Z` and `\B` are anchors outside a class and "bad
            // escape" inside one.
            r"[\A]",
            r"[\Z]",
            // Group references Python cannot resolve; JavaScript reads an
            // unmatched `\8` as a literal `8`.
            r"\1",
            r"\.\-\/\1",
            r"\8",
            r"(a)\12",
            // Python requires a fixed-width lookbehind; ES2018 does not.
            r"(?<=a|bc)d",
            r"(?<=ab+)c",
            r"(?<=a*)b",
            r"(?<=a+)b",
            r"(?<=(a+))b",
            // `sre` cannot size a group reference, so a backreference inside a
            // lookbehind is rejected even when the group is fixed width.
            r"(a+)(?<=\1)b",
            r"(a)(?<=\1)b",
            // An octal escape may not exceed `0o377`; JavaScript reads `\400`
            // as `\40` followed by `0`.
            r"\400",
            r"[\400]",
            // `\N{…}` names a character in Python and is a literal `N` in
            // JavaScript, so the two never agree on what the pattern means.
            r"\N{BULLET}",
            r"\N{NOT_A_UNICODE_NAME}",
            // `chr()` caps a `\U` escape at `0x10FFFF`.
            r"\U00110000",
            // `\8`/`\9` are not octal, and a class has no group references.
            r"[\8]",
            r"[\9]",
            // At or above `MAXREPEAT` the repetition number is too large.
            r"a{4294967295}",
            r"a{4294967296}",
            // Python cannot quantify a zero-width anchor ("nothing to
            // repeat"); `\A` is a literal `A` in JavaScript.
            r"\A*",
            r"\Z+",
            r"\b?",
            r"\A{2}",
            r"^*",
            r"$*",
            // Unbalanced structure.
            r"(a",
            r"[a",
            r"abc)",
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
            r"\u0041\x41\U00000041",
            r"\.\-\/",
            // A backreference to a group that exists, and three octal digits
            // (which Python reads as a character, not a group reference).
            r"(a)\1",
            r"\101",
            // The top of the octal range, and a lookbehind that is fixed
            // width and carries no backreference.
            r"\377",
            r"(a)(?<=b)c",
            // The top `\U` code point, the largest legal bound, and braces
            // Python reads as literals rather than quantifiers.
            r"\U0010FFFF",
            r"a{4294967294}",
            r"a{1,3}",
            r"a{2,}",
            r"a{foo}",
            r"a{",
            // An anchor is fine unquantified, a brace after one is a literal,
            // and Python does allow a quantified lookahead.
            r"\Aa",
            r"a\Z",
            r"\A{foo}",
            r"(?=a)*",
            r"\d*",
            r"[\b]*",
            // `\b` is a backspace inside a class, and `\1` an octal escape.
            r"[\b]",
            r"[\1]",
            // `-` as the last member, and a class that merely lists `(?<`.
            r"[a-z-.]",
            r"[\-a]",
            r"[(?<]",
            r"[]]",
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

    /// The walk mirrors the validator: a `pattern` under an applicator keyword
    /// is stripped, while one under an annotation or an `x-` extension is data
    /// the meta-schema never treats as a schema, so it survives untouched.
    #[test]
    fn walks_applicators_and_leaves_extension_data_alone() {
        let bad = r"\p{L}";
        let mut schema = json!({
            "items": {"pattern": bad},
            "prefixItems": [{"pattern": bad}],
            "contains": {"pattern": bad},
            "unevaluatedItems": {"pattern": bad},
            "additionalItems": {"pattern": bad},
            "additionalProperties": {"pattern": bad},
            "unevaluatedProperties": {"pattern": bad},
            "propertyNames": {"pattern": bad},
            "allOf": [{"pattern": bad}],
            "oneOf": [{"pattern": bad}],
            "not": {"pattern": bad},
            "if": {"pattern": bad},
            "then": {"pattern": bad},
            "else": {"pattern": bad},
            "contentSchema": {"pattern": bad},
            "x-metadata": {"pattern": bad},
            "x-vendor": {"nested": {"pattern": bad}}
        });
        strip_unsupported_patterns(&mut schema);
        assert_eq!(
            schema,
            json!({
                "items": {},
                "prefixItems": [{}],
                "contains": {},
                "unevaluatedItems": {},
                "additionalItems": {},
                "additionalProperties": {},
                "unevaluatedProperties": {},
                "propertyNames": {},
                "allOf": [{}],
                "oneOf": [{}],
                "not": {},
                "if": {},
                "then": {},
                "else": {},
                "contentSchema": {},
                "x-metadata": {"pattern": bad},
                "x-vendor": {"nested": {"pattern": bad}}
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
