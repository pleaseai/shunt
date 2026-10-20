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
//! ([`thinking_signature::wrap_upstream_signature`]). The envelope names the
//! upstream by a digest of its `base_url`, never the URL itself: the envelope
//! is client-visible and persists in transcripts, and a `base_url` can carry
//! credentials. The envelope is a shunt-minted form, so the existing strip
//! drops it on the way to any other upstream; only a request going back to the
//! provider whose `base_url` digests to the envelope's key gets the original
//! signature back, byte for byte, before that strip runs
//! ([`prepare_thinking`]). No session state is involved, so
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
//! immediately before that block's `content_block_stop`. Withholding is
//! bounded ([`signature_ceiling`]); a signature that outgrows the bound ends
//! the stream with an `error` event rather than leave shunt raw.
//!
//! `redacted_thinking` (`data`) is deliberately left alone: it is the same
//! class of host-bound opaque value, but nothing here has seen a third-party
//! upstream emit one, and the strip does not recognise it either.

use std::collections::HashMap;
use std::fmt::Write as _;

use axum::body::Bytes;
use axum::http::{header::ACCEPT_ENCODING, HeaderMap, HeaderValue};
use futures_util::{stream, Stream, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::sse_data::{frame_data, replace_data};
use crate::config::{AuthMode, ProviderConfig};
use crate::model::thinking_signature::{unwrap_upstream_signature, wrap_upstream_signature};
use crate::request::RequestBody;

/// Ceiling on the signature bytes one stream may withhold at a time.
///
/// Callers do not name this directly — they ask [`signature_ceiling`] for the
/// bound that applies to them.
///
/// It is not the first-frame ceiling: Anthropic documents a signature as an
/// encrypted copy of the full reasoning, so a legitimate one can be far larger
/// than the 64 KiB a frame is otherwise allowed to grow to. 8 MiB is more than
/// ten times any plausible signature and still bounds what a stream that never
/// stops sending signature bytes can make the gateway hold.
const MAX_SIGNATURE_BYTES: usize = 8 * 1024 * 1024;

/// What one open block's entry is charged against `max_signature` on top of
/// its bytes: roughly a map slot plus a string's header and smallest
/// allocation. Without it, one-byte fragments across distinct indices that
/// never stop would hold far more memory than the bound counts.
const SIGNATURE_ENTRY_COST: usize = 64;

/// The withheld-signature ceiling for a caller holding `response_byte_cap`,
/// mirroring [`super::model_rewrite::first_frame_ceiling`].
///
/// The cap matters because withheld bytes are emitted later, wrapped, and
/// count against it then: holding more than the caller will ever accept is
/// spending the memory the cap exists to deny. `None` is the client path,
/// bounded by the constant alone.
pub(super) fn signature_ceiling(response_byte_cap: Option<usize>) -> usize {
    response_byte_cap.map_or(MAX_SIGNATURE_BYTES, |cap| cap.min(MAX_SIGNATURE_BYTES))
}

/// The JSON key that marks a frame as signature-bearing, quotes included so
/// `"signature_delta"` — a value, and a different word — does not match.
const SIGNATURE_KEY: &[u8] = b"\"signature\"";

/// Most spaces or tabs a key may have between its closing quote and its `:`.
/// A match is a key only when the `:` follows within this gap, which is what
/// tells `"signature":` from a string value reading `signature`. A key with a
/// longer gap is valid JSON but is not recognised, an accepted residual.
const MAX_KEY_GAP: usize = 4;

/// Bytes of the forwarded frame carried between chunks while skipping: the
/// byte before a [`SIGNATURE_KEY`] (which tells a key from an escaped quote),
/// the key itself, and up to [`MAX_KEY_GAP`] gap bytes, so a key whose `:` has
/// not arrived yet is examined again with the next chunk. A straddling event
/// boundary needs only three.
const SKIP_TAIL: usize = 1 + SIGNATURE_KEY.len() + MAX_KEY_GAP;

/// The envelope key for a third-party provider's signatures, or `None` when
/// this provider's responses must not be wrapped.
///
/// A `claude_oauth` pool is Anthropic by construction — its accounts are
/// subscription logins, whatever `base_url` a test or a relay hop points it
/// at — and a provider on `api.anthropic.com` is Anthropic by host
/// ([`super::safeguards::is_first_party`]). Everything else is third-party.
///
/// The key is a digest of the `base_url` with any trailing `/` trimmed, so a
/// provider written either way issues and accepts the same envelopes. It is
/// the lowercase hex of the first 16 bytes of the SHA-256 of that string: the
/// envelope reaches every client and persists in transcripts, and a `base_url`
/// can carry userinfo or a path token, so the URL itself must not be exposed.
/// No secret salt: the key must stay stable across restarts and across shunt
/// instances sharing a `base_url`. The cost is that the digest confirms a
/// guess: whoever can guess the whole `base_url`, credentials included, can
/// check it against an envelope.
pub(super) fn signing_key(provider: &ProviderConfig) -> Option<String> {
    (provider.auth != AuthMode::ClaudeOauth
        && !super::safeguards::is_first_party(&provider.base_url))
    .then(|| {
        let digest = Sha256::digest(provider.base_url.trim_end_matches('/').as_bytes());
        digest[..16]
            .iter()
            .fold(String::with_capacity(32), |mut key, byte| {
                let _ = write!(key, "{byte:02x}");
                key
            })
    })
}

/// Ask a third-party upstream for an uncompressed reply.
///
/// The client's `Accept-Encoding` is otherwise forwarded, and shunt's HTTP
/// client decodes no content coding, so a compressed reply would reach the
/// wrap still encoded: unparseable, it would pass through with its signature
/// raw. `identity` rather than removing the header, since a request without
/// one leaves the coding to the server. Every other provider keeps the
/// client's header, as passthrough.
pub(super) fn request_identity_encoding(headers: &mut HeaderMap, provider: &ProviderConfig) {
    if signing_key(provider).is_some() {
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    }
}

/// Prepare a request's thinking blocks for `provider`: strip every foreign
/// signature except this provider's own envelopes, then restore those.
///
/// The strip runs first so a restored value that is itself shunt-minted — this
/// upstream being another shunt that wrapped its own third-party signature —
/// survives to reach the upstream that can unwrap it. Envelopes naming any
/// other upstream, and every other shunt-minted signature, are still dropped.
pub(super) fn prepare_thinking(body: &mut RequestBody, provider: &ProviderConfig) {
    match signing_key(provider) {
        Some(key) => {
            super::thinking::strip_foreign_thinking_except(body, |signature| {
                unwrap_upstream_signature(signature).is_some_and(|(upstream, _)| upstream == key)
            });
            restore_own_signatures(body, &key);
        }
        None => super::thinking::strip_foreign_thinking(body),
    }
}

/// Restore the original signature on every thinking block whose envelope names
/// `key`, a provider's [`signing_key`]. [`prepare_thinking`] runs it after the
/// strip, which spares these envelopes and drops the rest.
///
/// A first-party or `claude_oauth` provider has no [`signing_key`], so
/// [`prepare_thinking`] never calls this for it and every envelope leaves with
/// the strip. An envelope naming any other upstream is likewise left wrapped
/// for the strip to drop.
pub(super) fn restore_own_signatures(body: &mut RequestBody, key: &str) {
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
/// Two bounds apply, because the two things held differ in size:
///
/// - `max_frame` bounds an incomplete frame that carries no signature, exactly
///   as [`super::model_rewrite::rewrite_first_model_stream`]'s ceiling bounds
///   its scan: a frame that outgrows it is forwarded as it stands, verbatim
///   through its own boundary, so a long text or thinking delta keeps
///   streaming.
/// - `max_signature` ([`signature_ceiling`]) bounds the signature bytes
///   withheld across every open block, each block also charged
///   [`SIGNATURE_ENTRY_COST`], together with an incomplete frame that
///   carries a `"signature"` key. Such a frame is not forwarded at `max_frame`
///   — that would hand the client the raw signature — but held until it
///   completes.
///
/// Neither bound gives way to a raw signature. A signature that outgrows
/// `max_signature`, or a `"signature"` key turning up in a frame already
/// being forwarded unread, fails closed: every withheld byte is dropped, the
/// client gets an Anthropic-shape `error` event (after a blank line, when half
/// a frame has already reached it), and the stream ends there.
pub(super) fn wrap_signature_stream<S, E>(
    upstream: S,
    key: Option<String>,
    max_frame: usize,
    max_signature: usize,
) -> impl Stream<Item = Result<Bytes, E>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    let machine = key.map(|key| SignatureWrap::new(key, max_frame, max_signature));
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
                        // Closed: `out` ends with the error event, and nothing
                        // the upstream sends after it reaches the client.
                        if machine.closed {
                            return Some((Ok(Bytes::from(out)), (upstream, None, true)));
                        }
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
    max_signature: usize,
    /// Bytes of the frame not yet complete.
    pending: Vec<u8>,
    /// How far into `pending` is known to hold no event boundary, so a large
    /// frame arriving in small chunks is searched once, not once per chunk.
    scanned: usize,
    /// Whether `pending`, having outgrown `max_frame`, is held to its boundary
    /// instead of forwarded, and how sure that is ([`Hold`]). A held frame's
    /// bytes count against `max_signature` either way. The key is looked for
    /// once, as the frame crosses `max_frame`; a provisional hold re-decides
    /// only its own match, and rescans only past it, as more bytes arrive.
    hold: Hold,
    /// Set after an oversized frame was forwarded unfinished: the bytes up to
    /// its boundary are passed through without being framed, watched only for
    /// a `"signature"` key and for that boundary.
    skipping: bool,
    /// The last [`SKIP_TAIL`] bytes forwarded while `skipping`, so a key, its
    /// `:`, or a boundary split across two chunks is still found.
    ///
    /// Invariant: it is only ever built from bytes before the skipped frame's
    /// boundary, so it never holds a complete event boundary. A boundary
    /// found in `tail` plus the next chunk's head therefore ends inside that
    /// chunk, and `end - carried` in [`Self::push`] cannot underflow. A key
    /// it holds whole was already examined: had it been one, the stream would
    /// have failed closed, so examining it again cannot fail twice, and one
    /// still undecided is decided now that more bytes follow it.
    tail: Vec<u8>,
    /// Withheld signature bytes per content block index.
    signatures: HashMap<u64, String>,
    /// Total length of every value in `signatures`, plus
    /// [`SIGNATURE_ENTRY_COST`] per entry.
    withheld: usize,
    /// Set once the stream has failed closed: nothing more is emitted.
    closed: bool,
}

impl SignatureWrap {
    fn new(key: String, max_frame: usize, max_signature: usize) -> Self {
        Self {
            key,
            max_frame,
            max_signature,
            pending: Vec::new(),
            scanned: 0,
            hold: Hold::Off,
            skipping: false,
            tail: Vec::new(),
            signatures: HashMap::new(),
            withheld: 0,
            closed: false,
        }
    }

    /// Consume one upstream chunk; return the bytes ready for the client.
    fn push(&mut self, mut chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        if self.closed {
            return out;
        }
        if self.skipping {
            // A boundary, or a key with its gap and `:`, straddling the chunk
            // edge ends within the chunk's first `SKIP_TAIL - 1` bytes, so
            // only those are joined to the carried tail; the rest of the chunk
            // is searched in place rather than copied.
            let mut edge = std::mem::take(&mut self.tail);
            let carried = edge.len();
            edge.extend_from_slice(&chunk[..chunk.len().min(SKIP_TAIL - 1)]);
            let edge_end = event_boundary(&edge);
            let end = edge_end
                .map(|end| end - carried)
                .or_else(|| event_boundary(chunk));
            // Only the skipped frame's own bytes are watched: what follows its
            // boundary is the next frame's, framed normally below. A match
            // whose lookahead runs out is no key here — at the boundary the
            // next bytes are `\r`/`\n`, never `:`, and at the end of the
            // chunk the match is carried in `tail` and decided next push.
            let straddle_from = carried.saturating_sub(SKIP_TAIL - 1);
            let straddle = &edge[straddle_from..edge_end.unwrap_or(edge.len())];
            let before_straddle = straddle_from.checked_sub(1).map(|i| edge[i]);
            if has_signature_key(straddle, before_straddle, Undecided::NotKey)
                || has_signature_key(
                    &chunk[..end.unwrap_or(chunk.len())],
                    edge[..carried].last().copied(),
                    Undecided::NotKey,
                )
            {
                self.fail(&mut out, true, SKIPPED_KEY_MESSAGE);
                return out;
            }
            match end {
                Some(end) => {
                    let (rest_of_frame, after) = chunk.split_at(end);
                    out.extend_from_slice(rest_of_frame);
                    self.skipping = false;
                    chunk = after;
                }
                None => {
                    out.extend_from_slice(chunk);
                    // A chunk shorter than the tail is wholly inside `edge`.
                    let last = if chunk.len() >= SKIP_TAIL {
                        chunk
                    } else {
                        &edge
                    };
                    self.tail = last[last.len().saturating_sub(SKIP_TAIL)..].to_vec();
                    return out;
                }
            }
        }
        // Frames are walked by offset and the consumed prefix removed once, so
        // a chunk of many small frames costs one pass, not one shift per frame.
        // The first search resumes where the last push stopped, backed up by
        // the three bytes a `\r\n\r\n` straddling that point can start with.
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend_from_slice(chunk);
        let mut start = 0;
        let mut from = self.scanned.saturating_sub(3);
        while let Some(len) = event_boundary(&pending[start + from..]) {
            let end = start + from + len;
            let frame = &pending[start..end];
            match self.rewrite(frame) {
                Some(rewritten) => out.extend_from_slice(&rewritten),
                None => out.extend_from_slice(frame),
            }
            if self.closed {
                self.fail(&mut out, false, BOUND_MESSAGE);
                return out;
            }
            start = end;
            from = 0;
        }
        if start > 0 {
            // The frame being held, if any, has completed.
            self.hold = Hold::Off;
        }
        pending.drain(..start);
        self.pending = pending;
        self.scanned = self.pending.len();
        if self.pending.len() > self.max_frame {
            self.hold = self.decide_hold();
            if self.hold == Hold::Off {
                self.tail = self.pending[self.pending.len().saturating_sub(SKIP_TAIL)..].to_vec();
                out.append(&mut self.pending);
                self.scanned = 0;
                self.skipping = true;
            } else if self.pending.len() + self.withheld > self.max_signature {
                self.fail(&mut out, false, BOUND_MESSAGE);
            }
        }
        out
    }

    /// Whether the oversized incomplete frame in `pending` is to be held.
    ///
    /// A match whose `:` has not arrived yet holds the frame provisionally:
    /// holding a frame that turns out to carry no key costs only time, while
    /// forwarding one that does would leak its signature. Each later push
    /// re-decides that match alone. Once it proves a value, the frame is
    /// searched again from just past it — the bytes before it held no key — and,
    /// with no key there either, it is forwarded and skipped like any frame
    /// without one, so it is buffered no longer than its key stays undecided.
    fn decide_hold(&self) -> Hold {
        let from = match self.hold {
            Hold::Off => 0,
            Hold::Confirmed => return Hold::Confirmed,
            Hold::Provisional(at) => match key_follows(&self.pending[at + SIGNATURE_KEY.len()..]) {
                Some(true) => return Hold::Confirmed,
                Some(false) => at + 1,
                None => return Hold::Provisional(at),
            },
        };
        match find_signature_key(&self.pending, from) {
            KeyMatch::Key => Hold::Confirmed,
            KeyMatch::Undecided(at) => Hold::Provisional(at),
            KeyMatch::Absent => Hold::Off,
        }
    }

    /// The stream ended: whatever incomplete frame is held goes out verbatim,
    /// so no byte the upstream sent is lost — unless it carries a
    /// `"signature"` key, which would hand the client a raw signature, or the
    /// stream already failed closed, after which nothing goes out. A frame
    /// cut off before its boundary is never dispatched by an SSE client, so
    /// dropping one costs the client nothing.
    fn finish(&mut self) -> Vec<u8> {
        let pending = std::mem::take(&mut self.pending);
        if has_signature_key(&pending, None, Undecided::Key) {
            return Vec::new();
        }
        pending
    }

    /// Fail closed: drop every withheld byte, append the error event to `out`,
    /// and emit nothing after it. `mid_frame` is set when part of a frame has
    /// already reached the client, which a blank line then terminates so the
    /// error is not read as the rest of that frame. `message` is the error's
    /// reason: [`BOUND_MESSAGE`] when a signature outgrew `max_signature`,
    /// [`SKIPPED_KEY_MESSAGE`] when a signature key turned up in a frame
    /// already being forwarded unread. It never carries upstream bytes.
    fn fail(&mut self, out: &mut Vec<u8>, mid_frame: bool, message: &str) {
        self.closed = true;
        self.pending = Vec::new();
        self.scanned = 0;
        self.hold = Hold::Off;
        self.skipping = false;
        self.tail = Vec::new();
        self.signatures = HashMap::new();
        self.withheld = 0;
        if mid_frame {
            out.extend_from_slice(b"\n\n");
        }
        let error = json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": message,
            },
        });
        out.extend_from_slice(format!("event: error\ndata: {error}\n\n").as_bytes());
    }

    /// Withhold `fragment` as part of block `index`'s signature, or fail closed
    /// (`false`) when that would take the stream past `max_signature`.
    ///
    /// An empty fragment withholds nothing and opens no entry, as an empty
    /// start signature does not: otherwise `"signature":""` deltas across
    /// distinct indices, never stopped, would grow `signatures` with nothing
    /// counted against the bound. A fragment that opens an entry is also
    /// charged [`SIGNATURE_ENTRY_COST`].
    fn withhold(&mut self, index: u64, fragment: &str) -> bool {
        if fragment.is_empty() {
            return true;
        }
        let opens = !self.signatures.contains_key(&index);
        let cost = fragment.len() + if opens { SIGNATURE_ENTRY_COST } else { 0 };
        if self.withheld + cost > self.max_signature {
            self.closed = true;
            return false;
        }
        self.withheld += cost;
        self.signatures.entry(index).or_default().push_str(fragment);
        true
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
                if !self.withhold(index, signature) {
                    return Some(Vec::new());
                }
                block["signature"] = Value::String(String::new());
                Some(replace_data(text, &event).into_bytes())
            }
            "content_block_delta" => {
                let delta = event.get("delta")?;
                if delta.get("type").and_then(Value::as_str) != Some("signature_delta") {
                    return None;
                }
                let fragment = delta.get("signature").and_then(Value::as_str)?;
                // Withheld or failed closed, nothing of this frame goes out.
                self.withhold(index, fragment);
                Some(Vec::new())
            }
            "content_block_stop" => {
                let signature = self.signatures.remove(&index)?;
                self.withheld -= signature.len() + SIGNATURE_ENTRY_COST;
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

const BOUND_MESSAGE: &str = "an upstream thinking signature exceeded the gateway's signature bound";
const SKIPPED_KEY_MESSAGE: &str =
    "an upstream frame carried a thinking signature past the gateway's frame bound";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Byte index just past the first SSE event boundary (`\n\n` or `\r\n\r\n`).
///
/// Both forms end on a `\n`, so one forward scan that stops at the first
/// boundary of either kind finds the earliest without reading past it.
fn event_boundary(buf: &[u8]) -> Option<usize> {
    (0..buf.len())
        .find(|&i| buf[i] == b'\n' && (buf[..i].ends_with(b"\n") || buf[..i].ends_with(b"\r\n\r")))
        .map(|i| i + 1)
}

/// How [`has_signature_key`] counts a match whose lookahead runs past the end
/// of the searched bytes, before its `:` or anything else could decide it.
#[derive(Clone, Copy)]
enum Undecided {
    /// Count it as a key: the conservative choice where the bytes are all
    /// there will be for now.
    Key,
    /// Count it as no key: the end is a frame boundary, which no `:` follows,
    /// or the match will be examined again once more bytes arrive.
    NotKey,
}

/// Whether `buf` holds a [`SIGNATURE_KEY`] used as a JSON key.
///
/// The decision is made by lookahead: a match is a key only when at most
/// [`MAX_KEY_GAP`] spaces or tabs and then `:` follow it. A string value that
/// reads `signature` — `"mode":"signature"`, `["a","signature"]`, `"mode":
/// "signature"` — is followed by `,`, `]` or `}` instead, whatever precedes
/// it, and a key must stay a key even right after `data:` on a continuation
/// line (`data:"signature":…`). `undecided` says how to count a match the
/// lookahead cannot settle within `buf`.
///
/// One lookbehind remains. `before` is the byte preceding `buf`, when there
/// is one. A key whose own text ends `\"signature` (`{"say \"signature": 1}`)
/// completes the eleven bytes with its closing quote after a backslash, and is
/// not the `signature` key; nor is `\"signature\"` inside a string, which the
/// eleven bytes never match.
///
/// Two valid forms are not recognised, as accepted residuals: a key followed
/// by more than [`MAX_KEY_GAP`] spaces or tabs before its `:`, and a key whose
/// `:` is on the next `data:` line (`"signature"\ndata: :`).
fn has_signature_key(buf: &[u8], before: Option<u8>, undecided: Undecided) -> bool {
    buf.windows(SIGNATURE_KEY.len())
        .enumerate()
        .any(|(i, window)| {
            let preceding = if i == 0 { before } else { Some(buf[i - 1]) };
            if window != SIGNATURE_KEY || preceding == Some(b'\\') {
                return false;
            }
            key_follows(&buf[i + SIGNATURE_KEY.len()..]).unwrap_or(match undecided {
                Undecided::Key => true,
                Undecided::NotKey => false,
            })
        })
}

/// Whether an oversized incomplete frame is being held ([`SignatureWrap::hold`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// Not held: the frame is still under `max_frame`, or carries no key.
    Off,
    /// Held on a match at this offset in `pending` whose `:` has not arrived:
    /// the next bytes may yet show it to be a string value.
    Provisional(usize),
    /// Held on a confirmed `"signature"` key, until the frame completes.
    Confirmed,
}

/// What [`find_signature_key`] found.
enum KeyMatch {
    /// A confirmed key.
    Key,
    /// A match at this offset whose lookahead runs past the end of the bytes.
    /// Nothing can follow it: an undecided match leaves fewer bytes after it
    /// than another match needs.
    Undecided(usize),
    /// No key, and no match still open.
    Absent,
}

/// The first [`SIGNATURE_KEY`] used as a key in `buf`, among matches starting
/// at or after `from`, by the rule [`has_signature_key`] states.
fn find_signature_key(buf: &[u8], from: usize) -> KeyMatch {
    let Some(searched) = buf.get(from..) else {
        return KeyMatch::Absent;
    };
    for (offset, window) in searched.windows(SIGNATURE_KEY.len()).enumerate() {
        let at = from + offset;
        if window != SIGNATURE_KEY || at.checked_sub(1).map(|i| buf[i]) == Some(b'\\') {
            continue;
        }
        match key_follows(&buf[at + SIGNATURE_KEY.len()..]) {
            Some(true) => return KeyMatch::Key,
            Some(false) => {}
            None => return KeyMatch::Undecided(at),
        }
    }
    KeyMatch::Absent
}

/// Whether the bytes after a [`SIGNATURE_KEY`] make it a key: `Some(true)`
/// for a `:` after at most [`MAX_KEY_GAP`] spaces or tabs, `Some(false)` for
/// anything else, `None` when `after` ends first.
fn key_follows(after: &[u8]) -> Option<bool> {
    for (gap, &byte) in after.iter().enumerate().take(MAX_KEY_GAP + 1) {
        match byte {
            b':' => return Some(true),
            b' ' | b'\t' if gap < MAX_KEY_GAP => {}
            _ => return Some(false),
        }
    }
    None
}

#[cfg(test)]
#[path = "thinking_wrap_tests.rs"]
mod tests;
