//! Wrap a third-party Anthropic-kind upstream's thinking signatures on the way
//! to the client, and unwrap them on the way back to that same upstream.
//!
//! An Anthropic-kind provider that is not `api.anthropic.com` — MiniMax, Kimi,
//! Zhipu, an API-key Anthropic-compatible gateway — is relayed untranslated, so
//! the `signature` on its thinking blocks reaches Claude Code verbatim and comes
//! back in `messages` on every later turn. Nothing about that value says which
//! host issued it, so when a later turn resolves to the Claude pool — a
//! `[models.upstream_model]` failover back to #1, a `/model` switch, a router
//! tier flip — [`super::thinking::strip_foreign_thinking`] took it for
//! Anthropic's own and forwarded it, and Anthropic answered with a 400 that
//! does not advance the failover chain (issue #783).
//!
//! The fix is stateless, mirroring the Responses round-trip encoding: such a
//! signature leaves shunt wrapped in an envelope naming its upstream
//! ([`thinking_signature::wrap_upstream_signature`]). The envelope is a
//! shunt-minted form, so the existing strip drops it on the way to any other
//! upstream; only a request going back to the provider whose `base_url` the
//! envelope names gets the original signature back, byte for byte, before that
//! strip runs ([`restore_own_signatures`]). No session state is involved, so
//! the round trip holds across reloads and sticky-binding expiry alike.
//!
//! **Which responses are wrapped** is [`signing_key`]'s call: not a
//! `claude_oauth` pool and not `api.anthropic.com`. First-party responses stay
//! byte-for-byte passthrough, which is what Anthropic's own signatures need.
//!
//! **Streaming.** A client concatenates a thinking block's
//! `content_block_start.content_block.signature` and every `signature_delta`
//! fragment, so wrapping fragments one by one would yield a concatenation
//! nothing can decode. [`wrap_signature_stream`] instead withholds a block's
//! signature bytes — only those; text and thinking deltas flow as their frames
//! complete — and emits the whole signature, wrapped, as one `signature_delta`
//! immediately before that block's `content_block_stop`.
//!
//! `redacted_thinking` (`data`) is deliberately left alone: it is the same
//! class of host-bound opaque value, but nothing here has seen a third-party
//! upstream emit one, and the strip does not recognise it either.

use std::collections::HashMap;

use axum::body::Bytes;
use futures_util::{stream, Stream, StreamExt};
use serde_json::{json, Value};

use crate::config::{AuthMode, ProviderConfig};
use crate::model::thinking_signature::{unwrap_upstream_signature, wrap_upstream_signature};
use crate::request::RequestBody;

/// The envelope key for a third-party provider's signatures, or `None` when
/// this provider's responses must not be wrapped.
///
/// A `claude_oauth` pool is Anthropic by construction — its accounts are
/// subscription logins, whatever `base_url` a test or a relay hop points it
/// at — and a provider on `api.anthropic.com` is Anthropic by host
/// ([`super::safeguards::is_first_party`]). Everything else is third-party.
///
/// The key is the `base_url` with any trailing `/` trimmed, so a provider
/// written either way issues and accepts the same envelopes.
pub(super) fn signing_key(provider: &ProviderConfig) -> Option<&str> {
    (provider.auth != AuthMode::ClaudeOauth
        && !super::safeguards::is_first_party(&provider.base_url))
    .then(|| provider.base_url.trim_end_matches('/'))
}

/// Restore the original signature on every thinking block whose envelope names
/// this provider, before the strip drops the rest.
///
/// A first-party or `claude_oauth` provider has no [`signing_key`], so nothing
/// is restored and every envelope leaves with the strip. An envelope naming any
/// other upstream is likewise left wrapped for the strip to drop.
pub(super) fn restore_own_signatures(body: &mut RequestBody, provider: &ProviderConfig) {
    let Some(key) = signing_key(provider) else {
        return;
    };
    // The same clone-avoidance as `thinking::strip_foreign_thinking`: the
    // read-only scan is far cheaper than the `Arc::make_mut` deep copy that
    // `mutate` makes before its closure can report "no change".
    if !thinking_blocks(body.json()).any(|block| own_original(block, key).is_some()) {
        return;
    }
    body.mutate(|request| {
        let mut changed = false;
        for block in thinking_blocks_mut(request) {
            if let Some(original) = own_original(block, key) {
                block["signature"] = Value::String(original);
                changed = true;
            }
        }
        changed
    });
}

fn thinking_blocks(request: &Value) -> impl Iterator<Item = &Value> {
    request
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
}

fn thinking_blocks_mut(request: &mut Value) -> impl Iterator<Item = &mut Value> {
    request
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(|message| message.get_mut("content").and_then(Value::as_array_mut))
        .flatten()
}

/// The original signature inside a thinking block's envelope, when that
/// envelope names `key`.
fn own_original(block: &Value, key: &str) -> Option<String> {
    if block.get("type").and_then(Value::as_str) != Some("thinking") {
        return None;
    }
    let signature = block.get("signature").and_then(Value::as_str)?;
    let (upstream, original) = unwrap_upstream_signature(signature)?;
    (upstream == key).then_some(original)
}

/// Wrap every non-empty thinking signature in a non-streaming Messages body.
///
/// A body that does not parse, carries no `content` array, or has nothing to
/// wrap is returned unchanged — byte for byte, not re-serialised. An empty
/// signature is left as it is: it is no host's to verify, and the strip
/// already recognises it.
pub(super) fn wrap_response_signatures(body: Bytes, key: &str) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) else {
        return body;
    };
    let mut changed = false;
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("thinking") {
            continue;
        }
        let Some(signature) = block.get("signature").and_then(Value::as_str) else {
            continue;
        };
        if signature.is_empty() {
            continue;
        }
        block["signature"] = Value::String(wrap_upstream_signature(key, signature));
        changed = true;
    }
    if !changed {
        return body;
    }
    serde_json::to_vec(&value).map(Bytes::from).unwrap_or(body)
}

/// Wrap the thinking signatures of an Anthropic SSE stream under `key`.
///
/// `key` of `None` forwards the stream untouched. Otherwise the stream is cut
/// into SSE frames (`\n\n` / `\r\n\r\n`, across arbitrary chunk boundaries);
/// a frame that carries no signature passes through byte-identical as soon as
/// it completes, so only a block's signature bytes — plus at most one
/// incomplete frame — are ever held.
///
/// `max_frame` bounds that incomplete frame, exactly as
/// [`super::model_rewrite::rewrite_first_model_stream`]'s ceiling bounds its
/// scan: a frame that outgrows it is forwarded as it stands, verbatim through
/// its own boundary, and never inspected. No signature frame comes near the
/// bound, so the cost of giving up is nothing, and holding an unbounded frame
/// is what the bound denies.
pub(super) fn wrap_signature_stream<S, E>(
    upstream: S,
    key: Option<String>,
    max_frame: usize,
) -> impl Stream<Item = Result<Bytes, E>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    let machine = key.map(|key| SignatureWrap::new(key, max_frame));
    stream::unfold(
        (Box::pin(upstream), machine, false),
        |(mut upstream, machine, done)| async move {
            if done {
                return None;
            }
            let Some(mut machine) = machine else {
                return upstream
                    .next()
                    .await
                    .map(|item| (item, (upstream, None, false)));
            };
            loop {
                match upstream.next().await {
                    Some(Ok(chunk)) => {
                        let out = machine.push(&chunk);
                        if out.is_empty() {
                            continue;
                        }
                        return Some((Ok(Bytes::from(out)), (upstream, Some(machine), false)));
                    }
                    // A transport error ends the stream. The incomplete frame
                    // still held is dropped rather than forwarded, as the model
                    // rewrite does with its first frame: half a frame followed
                    // by an error is unparseable to the client either way.
                    Some(Err(error)) => return Some((Err(error), (upstream, None, true))),
                    None => {
                        let rest = machine.finish();
                        return (!rest.is_empty())
                            .then(|| (Ok(Bytes::from(rest)), (upstream, None, true)));
                    }
                }
            }
        },
    )
}

/// The frame-level state behind [`wrap_signature_stream`].
struct SignatureWrap {
    key: String,
    max_frame: usize,
    /// Bytes of the frame not yet complete.
    pending: Vec<u8>,
    /// Set after an oversized frame was forwarded unfinished: the bytes up to
    /// its boundary are passed through without being framed.
    skipping: bool,
    /// The last bytes forwarded while `skipping`, so a boundary split across
    /// two chunks is still found.
    tail: Vec<u8>,
    /// Withheld signature bytes per content block index.
    signatures: HashMap<u64, String>,
}

impl SignatureWrap {
    fn new(key: String, max_frame: usize) -> Self {
        Self {
            key,
            max_frame,
            pending: Vec::new(),
            skipping: false,
            tail: Vec::new(),
            signatures: HashMap::new(),
        }
    }

    /// Consume one upstream chunk; return the bytes ready for the client.
    fn push(&mut self, mut chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        if self.skipping {
            let mut joined = std::mem::take(&mut self.tail);
            let carried = joined.len();
            joined.extend_from_slice(chunk);
            match event_boundary(&joined) {
                Some(end) => {
                    let (rest_of_frame, after) = chunk.split_at(end - carried);
                    out.extend_from_slice(rest_of_frame);
                    self.skipping = false;
                    chunk = after;
                }
                None => {
                    out.extend_from_slice(chunk);
                    self.tail = joined[joined.len().saturating_sub(3)..].to_vec();
                    return out;
                }
            }
        }
        self.pending.extend_from_slice(chunk);
        while let Some(end) = event_boundary(&self.pending) {
            let frame: Vec<u8> = self.pending.drain(..end).collect();
            match self.rewrite(&frame) {
                Some(rewritten) => out.extend_from_slice(&rewritten),
                None => out.extend_from_slice(&frame),
            }
        }
        if self.pending.len() > self.max_frame {
            self.tail = self.pending[self.pending.len().saturating_sub(3)..].to_vec();
            out.append(&mut self.pending);
            self.skipping = true;
        }
        out
    }

    /// The stream ended: whatever incomplete frame is held goes out verbatim,
    /// so no byte the upstream sent is lost.
    fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    /// The bytes to forward in place of one complete frame, or `None` to
    /// forward it untouched.
    fn rewrite(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        // Most frames are text or thinking deltas: decide on bytes alone
        // whether this one can matter before parsing it.
        let interesting = contains(frame, b"signature")
            || (!self.signatures.is_empty() && contains(frame, b"content_block_stop"));
        if !interesting {
            return None;
        }
        let text = std::str::from_utf8(frame).ok()?;
        let mut event = frame_data(text)?;
        let index = event.get("index").and_then(Value::as_u64)?;
        match event.get("type").and_then(Value::as_str)? {
            "content_block_start" => {
                let block = event.get_mut("content_block")?;
                if block.get("type").and_then(Value::as_str) != Some("thinking") {
                    return None;
                }
                let signature = block.get("signature").and_then(Value::as_str)?;
                if signature.is_empty() {
                    return None;
                }
                self.signatures
                    .entry(index)
                    .or_default()
                    .push_str(signature);
                block["signature"] = Value::String(String::new());
                Some(replace_data(text, &event).into_bytes())
            }
            "content_block_delta" => {
                let delta = event.get("delta")?;
                if delta.get("type").and_then(Value::as_str) != Some("signature_delta") {
                    return None;
                }
                let fragment = delta.get("signature").and_then(Value::as_str)?;
                self.signatures.entry(index).or_default().push_str(fragment);
                Some(Vec::new())
            }
            "content_block_stop" => {
                let signature = self.signatures.remove(&index)?;
                if signature.is_empty() {
                    return None;
                }
                let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
                let delta = json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "signature_delta",
                        "signature": wrap_upstream_signature(&self.key, &signature),
                    },
                });
                let mut out =
                    format!("event: content_block_delta{newline}data: {delta}{newline}{newline}")
                        .into_bytes();
                out.extend_from_slice(frame);
                Some(out)
            }
            _ => None,
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Byte index just past the first SSE event boundary (`\n\n` or `\r\n\r\n`).
fn event_boundary(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| p + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The JSON payload of a frame's `data:` line.
fn frame_data(frame: &str) -> Option<Value> {
    frame.lines().find_map(|line| {
        let payload = line.trim_end_matches('\r').strip_prefix("data:")?;
        serde_json::from_str(payload.trim_start()).ok()
    })
}

/// The frame with its `data:` line replaced by `event`, every other line and
/// every line ending kept as it was.
fn replace_data(frame: &str, event: &Value) -> String {
    let mut out = String::with_capacity(frame.len());
    let mut replaced = false;
    for segment in frame.split_inclusive('\n') {
        let content_len = segment.trim_end_matches(['\n', '\r']).len();
        let (content, ending) = segment.split_at(content_len);
        if !replaced && content.starts_with("data:") {
            out.push_str("data: ");
            out.push_str(&event.to_string());
            out.push_str(ending);
            replaced = true;
        } else {
            out.push_str(segment);
        }
    }
    out
}

#[cfg(test)]
#[path = "thinking_wrap_tests.rs"]
mod tests;
