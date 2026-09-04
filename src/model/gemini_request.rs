//! Anthropic Messages -> Gemini generateContent request translation.

use axum::response::IntoResponse;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::adapters::AdapterError;

const MAX_SCHEMA_DEPTH: usize = 64;
const GEMINI_3_MODEL_PREFIX: &str = "gemini-3";
const GEMINI_TOOL_USE_ID_PREFIX: &str = "call_gemini_v1_";
// Google documents this exact value for imported/custom Gemini 3 function-call history.
const GEMINI_THOUGHT_SIGNATURE_PLACEHOLDER: &str = "context_engineering_is_the_way to_go";
/// Budget an enabled `thinking` block asks for when it names none. Shared with
/// `crate::model::antigravity_request`, which reads the same block to pick an
/// effort tier — the two must not drift apart on what "enabled, no budget"
/// means.
pub(crate) const DEFAULT_THINKING_BUDGET: u64 = 1024;

fn bad_request(message: impl Into<String>) -> AdapterError {
    AdapterError {
        message: message.into(),
        response: Box::new(axum::http::StatusCode::BAD_REQUEST.into_response()),
        failure: None,
    }
}

/// Translate an Anthropic Messages request into a Gemini `generateContent` request body.
///
/// Uses the request's model id for model-specific history validation. Adapters with
/// an alias should call [`translate_request_for_model`] with the resolved upstream id.
pub fn translate_request(request: &Value) -> Result<Value, AdapterError> {
    let model = request.get("model").and_then(Value::as_str).unwrap_or("");
    translate_request_for_model(request, model)
}

/// Translate a request using the resolved Gemini model id.
pub fn translate_request_for_model(request: &Value, model: &str) -> Result<Value, AdapterError> {
    let mut out = Map::new();

    // 1. System instruction
    if let Some(system_instruction) = translate_system_instruction(request) {
        out.insert("systemInstruction".to_string(), system_instruction);
    }

    // 2. Contents (multi-turn history)
    let contents = translate_messages(request, model)?;
    out.insert("contents".to_string(), Value::Array(contents));

    // 3. Generation Config
    let mut gen_config = translate_generation_config(request);
    if let Some(thinking_config) = translate_thinking_config(request) {
        gen_config.insert("thinkingConfig".to_string(), thinking_config);
    }
    if !gen_config.is_empty() {
        out.insert("generationConfig".to_string(), Value::Object(gen_config));
    }

    // 4. Tools & Tool Choice
    if let Some(tools) = translate_tools(request)? {
        out.insert("tools".to_string(), tools);
    }
    if let Some(tool_config) = translate_tool_config(request) {
        out.insert("toolConfig".to_string(), tool_config);
    }

    Ok(Value::Object(out))
}

/// Wrap a Gemini `generateContent` request in the Google Code Assist envelope.
pub fn wrap_code_assist_envelope(model: &str, project: &str, request: Value) -> Value {
    json!({
        "model": model,
        "project": project,
        "request": request
    })
}

fn translate_system_instruction(request: &Value) -> Option<Value> {
    let system = request.get("system")?;
    let mut parts = Vec::new();

    match system {
        Value::String(text) if !text.is_empty() => {
            parts.push(json!({ "text": text }));
        }
        Value::Array(blocks) => {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            parts.push(json!({ "text": text }));
                        }
                    }
                }
            }
        }
        _ => {}
    }

    if parts.is_empty() {
        None
    } else {
        Some(json!({ "parts": parts }))
    }
}

fn translate_messages(request: &Value, model: &str) -> Result<Vec<Value>, AdapterError> {
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut contents = Vec::new();
    let mut tool_names = HashMap::new();

    for message in messages {
        let role = match message.get("role").and_then(Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "model",
            _ => "user",
        };

        let mut parts = Vec::new();
        let mut saw_function_call = false;

        if let Some(content) = message.get("content") {
            match content {
                Value::String(text) => {
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
                Value::Array(blocks) => {
                    for block in blocks {
                        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    if !text.is_empty() {
                                        parts.push(json!({ "text": text }));
                                    }
                                }
                            }
                            "image" => {
                                let source = block
                                    .get("source")
                                    .ok_or_else(|| bad_request("image block is missing source"))?;
                                match source.get("type").and_then(Value::as_str) {
                                    Some("base64") => {
                                        let media_type = source
                                            .get("media_type")
                                            .and_then(Value::as_str)
                                            .unwrap_or("image/png");
                                        let data = source
                                            .get("data")
                                            .and_then(Value::as_str)
                                            .filter(|data| !data.is_empty())
                                            .ok_or_else(|| {
                                                bad_request("base64 image source is missing data")
                                            })?;
                                        parts.push(json!({
                                            "inlineData": {
                                                "mimeType": media_type,
                                                "data": data
                                            }
                                        }));
                                    }
                                    Some("url") => {
                                        return Err(bad_request(
                                            "URL image sources are not supported by the Gemini adapter",
                                        ));
                                    }
                                    _ => {
                                        return Err(bad_request(
                                            "unsupported image source type for Gemini",
                                        ));
                                    }
                                }
                            }
                            "tool_use" => {
                                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                                let input =
                                    block.get("input").cloned().unwrap_or_else(|| json!({}));
                                if !name.is_empty() {
                                    let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                                    if !id.is_empty() {
                                        tool_names.insert(id.to_string(), name.to_string());
                                    }
                                    let signature = decode_tool_use_signature(id).or_else(|| {
                                        (!saw_function_call
                                            && model.starts_with(GEMINI_3_MODEL_PREFIX))
                                        .then(|| GEMINI_THOUGHT_SIGNATURE_PLACEHOLDER.to_string())
                                    });
                                    let mut part = json!({
                                        "functionCall": {
                                            "name": name,
                                            "args": input
                                        }
                                    });
                                    if let Some(signature) = signature {
                                        part["thoughtSignature"] = Value::String(signature);
                                    }
                                    parts.push(part);
                                    saw_function_call = true;
                                }
                            }
                            "tool_result" => {
                                let tool_use_id = block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown_tool");
                                let name = tool_names
                                    .get(tool_use_id)
                                    .map(String::as_str)
                                    .unwrap_or("unknown_tool");
                                let output_val = extract_tool_result_content(block)?;
                                let mut response = Map::new();
                                response.insert("output".to_string(), output_val);
                                if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                                    response.insert("error".to_string(), Value::Bool(true));
                                }
                                parts.push(json!({
                                    "functionResponse": {
                                        "name": name,
                                        "response": response
                                    }
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        if !parts.is_empty() {
            push_content(&mut contents, role, parts);
        }
    }

    Ok(contents)
}

/// Append a translated turn, merging consecutive `user` turns into one.
///
/// Claude Code's `mid-conversation-system-2026-04-07` beta inserts `system`-role
/// messages into `messages`, including between an assistant `tool_use` and the
/// user's `tool_result`. Those fold to `user` here, which would otherwise emit a
/// standalone text turn between a `functionCall` and its `functionResponse` —
/// the Gemini API rejects that ordering. Merging keeps the reminder text and the
/// `functionResponse` in the same turn. `model` turns are never merged: shifting
/// their part indices would misalign thought signatures.
fn push_content(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if role == "user" {
        if let Some(previous) = contents
            .last_mut()
            .filter(|previous| previous["role"] == "user")
        {
            if let Some(existing) = previous.get_mut("parts").and_then(Value::as_array_mut) {
                existing.extend(parts);
                return;
            }
        }
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

fn decode_tool_use_signature(id: &str) -> Option<String> {
    let encoded = id.strip_prefix(GEMINI_TOOL_USE_ID_PREFIX)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    String::from_utf8(bytes)
        .ok()
        .filter(|signature| !signature.is_empty())
}

fn extract_tool_result_content(block: &Value) -> Result<Value, AdapterError> {
    let output = if let Some(content) = block.get("content") {
        match content {
            Value::String(text) => json!(text),
            Value::Array(blocks) => {
                let mut text_parts = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                text_parts.push(text);
                            }
                        }
                        Some("image") | Some("document") => {
                            return Err(bad_request(
                                "rich media tool results are not supported by the Gemini adapter",
                            ));
                        }
                        _ => {}
                    }
                }
                json!(text_parts.join("\n"))
            }
            _ => content.clone(),
        }
    } else {
        json!("")
    };
    Ok(output)
}

fn translate_generation_config(request: &Value) -> Map<String, Value> {
    let mut config = Map::new();

    if let Some(temp) = request.get("temperature").and_then(Value::as_f64) {
        config.insert("temperature".to_string(), json!(temp));
    }
    if let Some(max_tokens) = request.get("max_tokens").and_then(Value::as_u64) {
        config.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }
    if let Some(top_p) = request.get("top_p").and_then(Value::as_f64) {
        config.insert("topP".to_string(), json!(top_p));
    }
    if let Some(top_k) = request.get("top_k").and_then(Value::as_u64) {
        config.insert("topK".to_string(), json!(top_k));
    }
    if let Some(stops) = request.get("stop_sequences").and_then(Value::as_array) {
        let stop_strings: Vec<&str> = stops.iter().filter_map(Value::as_str).collect();
        if !stop_strings.is_empty() {
            config.insert("stopSequences".to_string(), json!(stop_strings));
        }
    }

    config
}

fn sanitize_gemini_schema(value: &mut Value, depth: usize) -> Result<(), AdapterError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(bad_request(
            "tool input schema exceeds maximum nesting depth",
        ));
    }
    match value {
        Value::Object(map) => {
            for key in [
                "$schema",
                "propertyNames",
                "$id",
                "$comment",
                "patternProperties",
                "exclusiveMinimum",
                "exclusiveMaximum",
                "const",
            ] {
                map.remove(key);
            }
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    // Maps of schemas keyed by names the *tool* chose. The
                    // values are schemas; the keys are not keywords, so a
                    // property called `const` or `enum` must be neither
                    // stripped nor skipped on account of its name.
                    "properties" | "$defs" | "definitions" | "dependentSchemas" => {
                        if let Value::Object(schemas) = child {
                            for schema in schemas.values_mut() {
                                sanitize_gemini_schema(schema, depth + 1)?;
                            }
                        }
                    }
                    // Also keyed by property names. `dependentRequired` maps
                    // each to a list of names — instances, left alone — and
                    // draft-07 `dependencies` maps each to either that list
                    // or a schema. Walking the map itself would read a
                    // property called `items` as the array keyword.
                    "dependentRequired" => {}
                    "dependencies" => {
                        if let Value::Object(entries) = child {
                            for entry in entries.values_mut().filter(|entry| entry.is_object()) {
                                sanitize_gemini_schema(entry, depth + 1)?;
                            }
                        }
                    }
                    // `default`, `example(s)` and `enum` hold *instances*, not
                    // schemas. Walking them would strip keys from — and, worse,
                    // invent an `items` inside — data the tool declared verbatim.
                    "default" | "example" | "examples" | "enum" => {}
                    _ => sanitize_gemini_schema(child, depth + 1)?,
                }
            }
            // After the children, so the tuple positions folded into `items`
            // below are already sanitized: an element schema that is itself an
            // array has its own `items` by now, and two positions that differ
            // only in a key just stripped are seen as the duplicates they are.
            flatten_type_list(map);
            drop_tuple_keywords_from_non_array(map);
            give_array_schema_items(map);
        }
        Value::Array(array) => {
            for child in array {
                sanitize_gemini_schema(child, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Make sure an array schema carries the one `items` schema the Gemini
/// backend requires, deriving it from a JSON Schema tuple when that is all
/// the tool declared.
///
/// The backend validates function declarations against its own `Schema`
/// message, and there an array without `items` is a hard `400` for the whole
/// request (`…properties[where].items.items: missing field.`). Tools written
/// against JSON Schema describe a fixed-shape array — `[field, operator,
/// value]` — as a tuple: `prefixItems` in 2020-12, an array-valued `items` in
/// draft-07. Neither is a Gemini `Schema` and neither leaves the array with the
/// single `items` schema it needs, so the positions are folded into one.
///
/// `Schema` also declares `type` REQUIRED on *every* node and has no union of
/// its own, so the folded schema has to carry a type: an `anyOf` node with no
/// `type` is the same missing-field failure one level down, and it is the shape
/// Google's own bridges collapse rather than forward. A position contributes
/// the type it declares — directly, through `anyOf`/`oneOf`/`allOf` arms, or
/// through what an `enum` of uniformly typed values or a `properties` map
/// implies; a position that declares nothing (the JSON
/// Schema "anything" slot) contributes nothing. The contributions collapse to
/// the single schema when they agree, to their shared `type` when they agree on
/// that much, and otherwise to the first position's schema. When no position
/// carries a type — or the array declares nothing about its elements at all —
/// the element is declared a string, matching CLIProxyAPI's fallback (see
/// `.please/docs/research/md/003-json-schema-sanitization-for-gemini-functiondeclar.md`).
/// An `items` schema that is present but declares no type is the same
/// missing field one level down. One whose type lives in composition arms,
/// or that sits beside `prefixItems`, is folded as one more position; `{}` —
/// the JSON Schema "anything" element — and its kin are otherwise given the
/// type they imply (through `enum` or `properties`) and failing that
/// `string`, as opencode's bridge does, with the rest of the schema kept.
/// Those last resorts trade precision for a
/// request that reaches the model at all: the cost is a narrowed element
/// type, not a rejected request.
///
/// A schema that declares `prefixItems` or `items` and no `type` at all is an
/// array in everything but name — JSON Schema applies both keywords only to
/// array instances, and a tool author who wrote one meant an array — so it is
/// given `type: "array"` before the same derivation runs, rather than losing
/// the tuple and reaching the backend as a schema that says nothing, or as one
/// whose array-valued `items` the backend rejects. Recognizing it is also what
/// lets a nested element schema be typed on its own pass, before the parent
/// array's fallback would otherwise declare it a string.
fn give_array_schema_items(map: &mut Map<String, Value>) {
    // Established before anything is removed: every other object in a schema
    // tree — a `properties` container above all — must leave here untouched.
    if !is_array_schema(map) {
        return;
    }
    // Positional and without a Gemini counterpart: folded in below, or dropped
    // when `items` already says what the elements are.
    let prefix_items = map.remove("prefixItems");
    map.entry("type").or_insert_with(|| json!("array"));
    // `items` counts as already answered only when it is a schema object,
    // and a typeless one is answered only once it has been given the type
    // every node needs. draft-07 spells a tuple as an array there and
    // 2020-12 closes one with `false`; neither is a schema Gemini can read.
    let legacy_tuple = match map.get_mut("items") {
        Some(Value::Object(items)) if items.contains_key("type") => return,
        // A union element declares its type through its arms, which is the
        // shape a tuple position already folds through, and an untyped
        // `items` beside `prefixItems` is how 2020-12 spells "and this after
        // the positions". Either way it is one more position: folded with the
        // rest, so the node Gemini sees carries a type and `{}` — "anything"
        // — contributes nothing rather than pulling the element to string.
        Some(Value::Object(items)) if has_composition(items) || prefix_items.is_some() => map
            .remove("items")
            .map(|element| Value::Array(vec![element])),
        Some(Value::Object(items)) => {
            give_element_schema_a_type(items);
            return;
        }
        Some(Value::Array(_)) => map.remove("items"),
        Some(_) => {
            map.remove("items");
            None
        }
        None => None,
    };
    let mut branches: Vec<Value> = Vec::new();
    for source in [prefix_items, legacy_tuple].into_iter().flatten() {
        if let Value::Array(positions) = source {
            for position in positions {
                collect_typed_branches(position, &mut branches);
            }
        }
    }
    let items = match branches.len() {
        0 => json!({ "type": "string" }),
        1 => branches.remove(0),
        _ => merge_branches(branches),
    };
    map.insert("items".to_string(), items);
}

/// The keywords whose arms a schema may declare its type through instead of
/// directly — the shape the tuple-position fold reads a type out of. The
/// gate in `give_array_schema_items` and the removal in
/// `collect_typed_branches` both read this list, so they cannot drift apart.
const COMPOSITION_KEYWORDS: [&str; 3] = ["anyOf", "oneOf", "allOf"];

fn has_composition(map: &Map<String, Value>) -> bool {
    COMPOSITION_KEYWORDS
        .iter()
        .any(|key| map.contains_key(*key))
}

/// The type a schema implies without declaring one: the one an `enum` of
/// uniformly typed values or a `properties` map is only meaningful for.
fn implied_type(map: &Map<String, Value>) -> Option<&'static str> {
    enum_value_type(map.get("enum"))
        .or_else(|| matches!(map.get("properties"), Some(Value::Object(_))).then_some("object"))
}

/// Give an element schema that declares no type — directly or through
/// composition arms, which the caller folds instead — the one Gemini
/// requires: the type it implies, and `string` when it implies none.
fn give_element_schema_a_type(items: &mut Map<String, Value>) {
    let kind = implied_type(items).unwrap_or("string");
    items.insert("type".to_string(), json!(kind));
}

/// Add the schemas one tuple position admits, skipping the ones that would
/// leave a branch without the `type` Gemini requires. A position speaks
/// through its own `type`, through the arms of a composition, or through
/// what its `enum` or `properties` imply, in that order — arms that name no
/// type hand back to the position itself rather than silencing it.
fn collect_typed_branches(mut position: Value, branches: &mut Vec<Value>) {
    let Some(map) = position.as_object_mut() else {
        return;
    };
    if map.contains_key("type") {
        push_distinct(branches, position);
        return;
    }
    // The position is owned and never emitted whole from here on, so its arms
    // are moved out rather than cloned. `allOf` is an intersection, not a
    // union, but its arms agree on the type when they name one, so the same
    // read serves; Gemini's `Schema` has no `allOf` to forward it as.
    // A node may carry more than one composition. The first whose arms name
    // a type speaks for the position; reading a later one as well would
    // merge arms that differ only in their constraints down to a bare type.
    let before = branches.len();
    for key in COMPOSITION_KEYWORDS {
        let Some(Value::Array(arms)) = map.remove(key) else {
            continue;
        };
        for arm in arms {
            collect_typed_branches(arm, branches);
        }
        if branches.len() > before {
            return;
        }
    }
    if let Some(kind) = implied_type(map) {
        push_distinct(branches, json!({ "type": kind }));
    }
}

fn push_distinct(branches: &mut Vec<Value>, schema: Value) {
    if !branches.contains(&schema) {
        branches.push(schema);
    }
}

/// The one `type` an `enum` of uniformly typed values implies, so a position
/// constrained by an enum alone is not mistaken for the "anything" slot.
fn enum_value_type(values: Option<&Value>) -> Option<&'static str> {
    let Value::Array(values) = values? else {
        return None;
    };
    let mut kind: Option<&'static str> = None;
    for value in values {
        let this = match value {
            Value::String(_) => "string",
            Value::Number(_) => "number",
            Value::Bool(_) => "boolean",
            _ => return None,
        };
        match kind {
            None => kind = Some(this),
            Some(seen) if seen == this => {}
            Some(_) => return None,
        }
    }
    kind
}

/// Collapse several element schemas into the one typed schema Gemini reads.
fn merge_branches(mut branches: Vec<Value>) -> Value {
    if let Some(shared) = branches[0].get("type").cloned() {
        // A bare `array` is the one type that is no schema on its own — it
        // needs `items` — so array positions keep the first, already
        // sanitized, branch instead of collapsing to the type.
        if shared != json!("array")
            && branches
                .iter()
                .all(|branch| branch.get("type") == Some(&shared))
        {
            return json!({ "type": shared });
        }
    }
    branches.remove(0)
}

/// Rewrite a JSON Schema type *list* as the scalar type plus `nullable`.
///
/// `Schema.type` is a single `Type` enum and nullability is its own `nullable`
/// field, so `{"type": ["array", "null"]}` is not a value the `parameters`
/// field can take — it is a `400` on `type` however good the rest of the
/// schema is. The list collapses the way the reference bridges collapse it
/// (CLIProxyAPI's `flattenTypeArrays`): the first non-`null` entry becomes the
/// type, and `null` among the entries becomes `nullable: true`. A list that
/// names nothing but `null` becomes a nullable string: the generativelanguage
/// proto does list a `NULL` member, but the Code Assist surface shunt talks to
/// is unverified there, so `string` + `nullable` is the fallback. A list that
/// names nothing usable is left alone rather than guessed at.
fn flatten_type_list(map: &mut Map<String, Value>) {
    let Some(Value::Array(kinds)) = map.get("type") else {
        return;
    };
    let mut nullable = false;
    let mut scalar: Option<Value> = None;
    for kind in kinds {
        match kind.as_str() {
            Some("null") => nullable = true,
            Some(_) if scalar.is_none() => scalar = Some(kind.clone()),
            _ => {}
        }
    }
    match scalar {
        Some(kind) => {
            map.insert("type".to_string(), kind);
            if nullable {
                map.insert("nullable".to_string(), json!(true));
            }
        }
        None if nullable => {
            map.insert("type".to_string(), json!("string"));
            map.insert("nullable".to_string(), json!(true));
        }
        None => {}
    }
}

/// `prefixItems`, draft-07's array-valued `items`, and 2020-12's boolean
/// `items` that closes a tuple describe array positions and nothing else, and
/// none of them is a shape Gemini's `Schema` can hold. On a schema whose type
/// — once `flatten_type_list` has settled a union such as `["string",
/// "array"]` on its first non-null member — is anything but `array`, they
/// would ride along and be rejected, so they go with the array member they
/// belonged to. An object-valued `items` is left as written: it is a `Schema`
/// field, so it is carried rather than refused.
fn drop_tuple_keywords_from_non_array(map: &mut Map<String, Value>) {
    if !matches!(map.get("type"), Some(Value::String(kind)) if kind != "array") {
        return;
    }
    map.remove("prefixItems");
    if !matches!(map.get("items"), None | Some(Value::Object(_))) {
        map.remove("items");
    }
}

fn is_array_schema(map: &Map<String, Value>) -> bool {
    match map.get("type") {
        Some(Value::String(kind)) => kind == "array",
        // A type list has been flattened to a scalar by the time this runs.
        Some(_) => false,
        // `prefixItems` and `items` — a draft-07 tuple or a single element
        // schema — each constrain arrays and nothing else, so a schema that
        // omits `type` but carries one of them is still an array schema, and
        // has to be seen as one here so its own elements get the treatment
        // above before a parent folds it in. Only a schema *keyword* counts:
        // the maps keyed by property names (`properties`, `$defs`,
        // `dependentSchemas`, `dependencies`, ...) never reach this function
        // because the walk above descends into their values, never the map
        // itself, and a boolean `items` says nothing about being an array.
        None => {
            matches!(map.get("prefixItems"), Some(Value::Array(_)))
                || matches!(map.get("items"), Some(Value::Array(_) | Value::Object(_)))
        }
    }
}

fn translate_tools(request: &Value) -> Result<Option<Value>, AdapterError> {
    let Some(tools) = request.get("tools").and_then(Value::as_array) else {
        return Ok(None);
    };
    if tools.is_empty() {
        return Ok(None);
    }

    let mut function_declarations = Vec::new();

    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut parameters = tool
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        sanitize_gemini_schema(&mut parameters, 0)?;

        function_declarations.push(json!({
            "name": name,
            "description": description,
            "parameters": parameters
        }));
    }

    if function_declarations.is_empty() {
        Ok(None)
    } else {
        Ok(Some(json!([{
            "functionDeclarations": function_declarations
        }])))
    }
}

fn translate_tool_config(request: &Value) -> Option<Value> {
    let tool_choice = request.get("tool_choice")?;
    let choice_type = tool_choice.get("type").and_then(Value::as_str)?;

    match choice_type {
        "auto" => Some(json!({
            "functionCallingConfig": {
                "mode": "AUTO"
            }
        })),
        "any" => Some(json!({
            "functionCallingConfig": {
                "mode": "ANY"
            }
        })),
        "tool" => {
            let name = tool_choice.get("name").and_then(Value::as_str)?;
            Some(json!({
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": [name]
                }
            }))
        }
        "none" => Some(json!({
            "functionCallingConfig": {
                "mode": "NONE"
            }
        })),
        _ => None,
    }
}

fn translate_thinking_config(request: &Value) -> Option<Value> {
    let thinking = request.get("thinking")?;
    match thinking.get("type").and_then(Value::as_str) {
        Some("enabled") => {
            let budget = thinking
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_THINKING_BUDGET);
            Some(json!({ "thinkingBudget": budget }))
        }
        Some("disabled") => Some(json!({ "thinkingBudget": 0 })),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
