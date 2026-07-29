use std::sync::Arc;

use serde_json::Value;

/// One buffered inbound request: exact client bytes plus their parsed JSON tree.
///
/// The raw bytes remain authoritative for passthrough adapters. Translating
/// adapters borrow `json`, so routing, flag extraction, and translation share the
/// one parse performed when the proxy finishes buffering the body.
#[derive(Clone, Debug)]
pub(crate) struct RequestBody {
    raw: Vec<u8>,
    json: Arc<Value>,
}

impl RequestBody {
    pub(crate) fn parse(raw: Vec<u8>) -> Result<Self, serde_json::Error> {
        let json = serde_json::from_slice(&raw)?;
        Ok(Self {
            raw,
            json: Arc::new(json),
        })
    }

    pub(crate) fn raw(&self) -> &[u8] {
        &self.raw
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

    /// Mutate the shared JSON representation and refresh the passthrough bytes
    /// only when the caller reports an observable change.
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

    use super::RequestBody;

    #[test]
    fn clones_share_json_but_preserve_raw_bytes() {
        let raw = b"{ \"model\": \"claude-test\", \"messages\": [] }".to_vec();
        let request = RequestBody::parse(raw.clone()).unwrap();
        let cloned = request.clone();

        assert!(Arc::ptr_eq(&request.json, &cloned.json));
        assert_eq!(request.raw(), raw);
        assert_eq!(cloned.raw(), raw);
    }

    #[test]
    fn no_op_mutation_keeps_original_bytes() {
        let raw = b"{ \"model\": \"claude-test\" }".to_vec();
        let mut request = RequestBody::parse(raw.clone()).unwrap();

        request.mutate(|_| false);

        assert_eq!(request.into_raw(), raw);
    }
}
