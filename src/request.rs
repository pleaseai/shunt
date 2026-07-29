use std::{fmt, sync::Arc};

use serde::{
    de::{self, MapAccess, SeqAccess, Visitor},
    Deserializer as _,
};
use serde_json::{Map, Number, Value};

/// One buffered inbound request: the passthrough bytes plus their parsed JSON tree.
///
/// `raw` is what passthrough adapters forward. It starts as the client's exact
/// bytes — which is what preserves byte-for-byte passthrough — but [`mutate`]
/// replaces it with a fresh serialization once a rewrite has to be observable
/// upstream, so it is authoritative rather than an audit copy of what the client
/// sent. Translating adapters borrow `json`, so routing, flag extraction, and
/// translation all share the one parse performed when the proxy finishes
/// buffering the body.
///
/// [`mutate`]: RequestBody::mutate
#[derive(Clone, Debug)]
pub(crate) struct RequestBody {
    raw: Vec<u8>,
    json: Arc<Value>,
}

struct TopLevelValueVisitor;

impl<'de> Visitor<'de> for TopLevelValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any valid JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("JSON numbers must be finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            // Check before pulling the value: a duplicate key ends the parse, so
            // there is no reason to materialize the `Value` that follows it. Keys
            // are moved rather than cloned — the inbound limit permits a
            // multi-megabyte key, so neither the map nor the error may copy one.
            if values.contains_key(&key) {
                return Err(de::Error::custom(format!(
                    "duplicate field `{}`",
                    echoed_key(&key)
                )));
            }
            let value = object.next_value::<Value>()?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

/// Longest client-supplied key echoed back in a parse error, in characters.
const MAX_ECHOED_KEY_CHARS: usize = 64;

/// Bound the key interpolated into a duplicate-key error. The message reaches the
/// client, and the inbound body limit allows a key far larger than any real field
/// name, so echoing it verbatim would let a malformed request inflate the error
/// response it gets back. Realistic field names are well under the cap and are
/// reproduced exactly.
fn echoed_key(key: &str) -> String {
    match key.char_indices().nth(MAX_ECHOED_KEY_CHARS) {
        Some((boundary, _)) => format!("{}...", &key[..boundary]),
        None => key.to_string(),
    }
}

impl RequestBody {
    /// Parse the request while rejecting duplicate keys in its top-level object.
    ///
    /// Shunt reads the parsed tree but may forward the original bytes verbatim;
    /// accepting a duplicate top-level key could make the gateway and upstream
    /// interpret the same request differently. Nested objects retain
    /// `serde_json::Value`'s last-value-wins behavior.
    ///
    /// This deliberately does not reproduce one `Value` behavior: because the
    /// `raw_value` feature is enabled in this build, stock `Value` deserialization
    /// treats a leading `$serde_json::private::RawValue` key as a sentinel and
    /// substitutes its contents. That is serde_json's private in-process protocol,
    /// never something an untrusted client should be able to trigger — and honoring
    /// it here would reintroduce exactly the raw-versus-tree divergence this type
    /// exists to prevent. The visitor treats it as an ordinary key.
    pub(crate) fn parse(raw: Vec<u8>) -> Result<Self, serde_json::Error> {
        let mut deserializer = serde_json::Deserializer::from_slice(&raw);
        let json = deserializer.deserialize_any(TopLevelValueVisitor)?;
        deserializer.end()?;
        Ok(Self {
            raw,
            json: Arc::new(json),
        })
    }

    pub(crate) fn json(&self) -> &Value {
        &self.json
    }

    pub(crate) fn json_arc(&self) -> Arc<Value> {
        Arc::clone(&self.json)
    }

    pub(crate) fn into_raw(self) -> Vec<u8> {
        self.raw
    }

    /// Mutate the shared JSON representation, refreshing the passthrough bytes
    /// only when the value actually changed.
    ///
    /// `update` **must** return `true` if and only if it mutated the value. That
    /// contract cannot be enforced here, and violating it is not benign: a closure
    /// that mutates and returns `false` leaves `raw` stale while `json` moves on,
    /// so routing and the managed-model policy would gate on one request while the
    /// passthrough adapter forwards a materially different one upstream.
    ///
    /// Callers that can cheaply rule out a change should check before calling —
    /// `Arc::make_mut` runs before `update` decides, so a no-op mutation on a body
    /// whose tree is shared still pays for a full copy-on-write clone.
    pub(crate) fn mutate(&mut self, update: impl FnOnce(&mut Value) -> bool) {
        if update(Arc::make_mut(&mut self.json)) {
            self.raw = serde_json::to_vec(self.json.as_ref())
                .expect("a serde_json::Value always serializes");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::Value;

    use super::RequestBody;

    #[test]
    fn duplicate_top_level_key_is_rejected() {
        let error =
            RequestBody::parse(br#"{"model":"first","model":"second"}"#.to_vec()).unwrap_err();

        assert!(error.to_string().contains("duplicate field `model`"));
    }

    #[test]
    fn duplicate_nested_key_collapses_to_last_value() {
        let request =
            RequestBody::parse(br#"{"tool":{"name":"first","name":"second"}}"#.to_vec()).unwrap();

        assert_eq!(request.json()["tool"]["name"], "second");
    }

    #[test]
    fn non_object_top_level_value_still_parses() {
        let request = RequestBody::parse(b"[1,2,3]".to_vec()).unwrap();

        assert_eq!(request.json(), &serde_json::json!([1, 2, 3]));
    }

    /// The inbound limit allows a key far larger than any real field name, and the
    /// parse error is returned to the client, so a duplicate must not let a
    /// malformed body inflate its own error response.
    #[test]
    fn duplicate_key_error_does_not_echo_an_oversized_key() {
        let key = "k".repeat(100_000);
        let body = format!(r#"{{"{key}":1,"{key}":2}}"#).into_bytes();

        let error = RequestBody::parse(body).unwrap_err().to_string();

        assert!(
            error.len() < 1_000,
            "duplicate-key error grew with the key: {} bytes",
            error.len()
        );
        assert!(
            error.contains("duplicate field"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn duplicate_key_error_reproduces_a_realistic_field_name_exactly() {
        let error = RequestBody::parse(br#"{"max_tokens":1,"max_tokens":2}"#.to_vec())
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("duplicate field `max_tokens`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn clones_share_json_but_preserve_raw_bytes() {
        let raw = b"{ \"model\": \"claude-test\", \"messages\": [] }".to_vec();
        let request = RequestBody::parse(raw.clone()).unwrap();
        let cloned = request.clone();

        assert!(Arc::ptr_eq(&request.json, &cloned.json));
        assert_eq!(request.raw, raw);
        assert_eq!(cloned.raw, raw);
    }

    #[test]
    fn no_op_mutation_keeps_original_bytes() {
        let raw = b"{ \"model\": \"claude-test\" }".to_vec();
        let mut request = RequestBody::parse(raw.clone()).unwrap();

        request.mutate(|_| false);

        assert_eq!(request.into_raw(), raw);
    }

    /// The property the Anthropic account pool depends on: one candidate's rewrite
    /// must not reach the retained body every other candidate is cloned from.
    #[test]
    fn mutating_a_clone_leaves_the_original_untouched() {
        let raw = b"{ \"model\": \"claude-test\", \"messages\": [] }".to_vec();
        let request = RequestBody::parse(raw.clone()).unwrap();
        let mut cloned = request.clone();

        cloned.mutate(|value| {
            value["model"] = Value::String("rewritten".to_string());
            true
        });

        assert!(!Arc::ptr_eq(&request.json, &cloned.json));
        assert_eq!(request.raw, raw);
        assert_eq!(request.json()["model"], "claude-test");
        assert_eq!(cloned.json()["model"], "rewritten");
        assert_ne!(cloned.raw, raw);
    }

    /// `json_arc` hands the tiktoken estimate a handle that outlives the adapter's
    /// own rewrites, so a later mutation must not retroactively change what it counts.
    #[test]
    fn a_handed_out_json_arc_still_observes_the_pre_mutation_tree() {
        let mut request = RequestBody::parse(b"{ \"model\": \"claude-test\" }".to_vec()).unwrap();
        let handed_out = request.json_arc();

        request.mutate(|value| {
            value["model"] = Value::String("rewritten".to_string());
            true
        });

        assert_eq!(handed_out["model"], "claude-test");
        assert_eq!(request.json()["model"], "rewritten");
    }

    #[test]
    fn no_op_mutation_on_a_shared_body_leaves_both_holders_identical() {
        let raw = b"{ \"model\": \"claude-test\" }".to_vec();
        let mut request = RequestBody::parse(raw.clone()).unwrap();
        let cloned = request.clone();

        request.mutate(|_| false);

        assert_eq!(request.raw, cloned.raw);
        assert_eq!(request.json(), cloned.json());
        assert_eq!(request.raw, raw);
    }
}
