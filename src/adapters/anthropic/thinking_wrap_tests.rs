#![allow(clippy::await_holding_lock)] // Intentional cross-module test serialization.
//! Non-vacuity. The wrap has two failure modes that look alike from one side:
//! wrapping nothing (the leak of issue #783 stays) and wrapping everything
//! (Anthropic's own signatures stop round-tripping). Every assertion that a
//! response *is* wrapped is paired with one that a first-party or
//! `claude_oauth` response is *not*, so forcing [`signing_key`] either way
//! turns one of them red. The request side is paired the same way: an
//! envelope restored for its own upstream, and one left for the strip
//! everywhere else.

use axum::body::Bytes;
use futures_util::{stream, FutureExt, StreamExt};
use serde_json::{json, Value};

use super::*;
use crate::config::{AuthMode, Config, ProviderConfig};
use crate::model::thinking_signature::{unwrap_upstream_signature, wrap_upstream_signature};

const THIRD_PARTY: &str = "https://api.minimax.cn/anthropic";

fn provider(base_url: &str, auth: AuthMode) -> ProviderConfig {
    let mut provider = Config::default()
        .provider("anthropic")
        .expect("built-in anthropic provider")
        .clone();
    provider.base_url = base_url.to_string();
    provider.auth = auth;
    provider
}

/// The envelope key a third-party provider at `base_url` signs with.
fn key_of(base_url: &str) -> String {
    signing_key(&provider(base_url, AuthMode::ApiKey)).expect("a third-party host has a key")
}

#[test]
fn only_a_third_party_provider_has_a_signing_key() {
    assert_eq!(
        signing_key(&provider(&format!("{THIRD_PARTY}/"), AuthMode::ApiKey)),
        signing_key(&provider(THIRD_PARTY, AuthMode::ApiKey)),
        "a trailing slash does not change the key"
    );
    assert_ne!(
        key_of(THIRD_PARTY),
        key_of("https://open.bigmodel.cn/api/anthropic"),
        "a third-party host signs its own thinking, keyed on its own base_url"
    );
    assert!(key_of(THIRD_PARTY).len() == 32 && !key_of(THIRD_PARTY).contains("minimax"));
    assert_eq!(
        signing_key(&provider(
            "https://api.anthropic.com",
            AuthMode::Passthrough
        )),
        None,
        "api.anthropic.com is the host the strip exists to protect"
    );
    assert_eq!(
        signing_key(&provider(THIRD_PARTY, AuthMode::ClaudeOauth)),
        None,
        "a claude_oauth pool is Anthropic whatever base_url it is pointed at"
    );
}

#[test]
fn the_signing_key_is_a_pinned_digest_of_the_trimmed_base_url() {
    // Keys must stay stable across shunt versions: an in-flight transcript
    // carries envelopes minted by an older build. Changing this value orphans
    // them, so it must never move silently.
    let expected = "ab6a8b83b9ed775e5633f5ff0525f5be";
    assert_eq!(key_of(THIRD_PARTY), expected);
    assert_eq!(key_of(&format!("{THIRD_PARTY}/")), expected);
}

#[test]
fn an_envelope_never_exposes_the_base_url_or_its_credentials() {
    let base = "https://user:secret@host.example/anthropic?token=abc";
    let key = key_of(base);
    let wrapped = wrap_upstream_signature(&key, "mm-sig");
    let decoded = String::from_utf8(
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &wrapped)
            .unwrap(),
    )
    .unwrap();
    for leaked in ["secret", "user", "token", "host.example"] {
        assert!(!decoded.contains(leaked), "{leaked} leaked into {decoded}");
    }
    // Twin: the same envelope still restores for that provider.
    let request = outbound(transcript(&wrapped), &provider(base, AuthMode::ApiKey));
    assert_eq!(first_block(&request)["signature"], "mm-sig");
}

// ---------------------------------------------------------------- request

fn body(request: Value) -> RequestBody {
    RequestBody::parse(serde_json::to_vec(&request).unwrap()).unwrap()
}

fn transcript(signature: &str) -> RequestBody {
    body(json!({
        "model": "MiniMax-M3",
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "weighing it", "signature": signature},
                {"type": "text", "text": "done"},
            ]},
        ],
    }))
}

fn first_block(request: &RequestBody) -> Value {
    request.json()["messages"][1]["content"][0].clone()
}

/// What every non-`claude_oauth` path runs on its way upstream.
fn outbound(mut request: RequestBody, to: &ProviderConfig) -> RequestBody {
    prepare_thinking(&mut request, to);
    request
}

#[test]
fn an_envelope_going_back_to_its_upstream_carries_the_original_signature() {
    let wrapped = wrap_upstream_signature(&key_of(THIRD_PARTY), "mm-sig+/=");
    let request = outbound(
        transcript(&wrapped),
        &provider(THIRD_PARTY, AuthMode::ApiKey),
    );
    let block = first_block(&request);
    assert_eq!(block["type"], "thinking");
    assert_eq!(
        block["signature"], "mm-sig+/=",
        "the host that issued the signature gets it back byte for byte"
    );
}

#[test]
fn a_nested_envelope_is_unwrapped_one_layer_and_not_stripped() {
    let inner = wrap_upstream_signature(&key_of("https://inner.example"), "orig");
    let nested = wrap_upstream_signature(&key_of(THIRD_PARTY), &inner);
    let request = outbound(
        transcript(&nested),
        &provider(THIRD_PARTY, AuthMode::ApiKey),
    );
    assert_eq!(
        first_block(&request)["signature"],
        inner,
        "the inner shunt must receive its own envelope intact"
    );
    // Twin: an envelope for a different upstream is still dropped.
    let other = wrap_upstream_signature(&key_of("https://other.example"), "orig");
    let request = outbound(transcript(&other), &provider(THIRD_PARTY, AuthMode::ApiKey));
    assert_eq!(first_block(&request)["type"], "text");
}

#[test]
fn prepare_thinking_strips_foreign_and_keeps_genuine_signatures() {
    let to = provider("https://api.example-gateway.test", AuthMode::ApiKey);
    let foreign = wrap_upstream_signature(&key_of(THIRD_PARTY), "mm-sig");
    let request = outbound(transcript(&foreign), &to);
    assert_eq!(first_block(&request)["type"], "text");
    let request = outbound(transcript("ErUBCkYIBBgCKkB"), &to);
    assert_eq!(first_block(&request)["signature"], "ErUBCkYIBBgCKkB");
}

#[test]
fn an_envelope_going_anywhere_else_is_dropped_by_the_strip() {
    let wrapped = wrap_upstream_signature(&key_of(THIRD_PARTY), "mm-sig");
    for (name, to) in [
        (
            "another third-party host",
            provider("https://open.bigmodel.cn/api/anthropic", AuthMode::ApiKey),
        ),
        (
            "api.anthropic.com",
            provider("https://api.anthropic.com", AuthMode::Passthrough),
        ),
        (
            "a claude_oauth pool",
            provider(THIRD_PARTY, AuthMode::ClaudeOauth),
        ),
    ] {
        let request = outbound(transcript(&wrapped), &to);
        assert_eq!(
            first_block(&request)["type"],
            "text",
            "{name} cannot verify MiniMax's signature, so the block must not reach it"
        );
    }
}

#[test]
fn a_genuine_signature_is_untouched_and_the_body_keeps_its_bytes() {
    // Spacing `serde_json::to_vec` would not reproduce, so a needless
    // re-serialisation by the restore is visible rather than coincidentally equal.
    let raw = br#"{ "messages": [{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"ErUBCkYIBBgCKkB"}]}] }"#;
    let mut request = RequestBody::parse(raw.to_vec()).unwrap();
    restore_own_signatures(&mut request, &key_of(THIRD_PARTY));
    assert_eq!(request.into_raw(), raw.to_vec());
}

// ------------------------------------------------------------ non-streaming

#[test]
fn a_buffered_response_has_each_thinking_signature_wrapped() {
    let raw = json!({
        "model": "MiniMax-M3",
        "content": [
            {"type": "thinking", "thinking": "a", "signature": "mm-sig"},
            {"type": "thinking", "thinking": "b", "signature": ""},
            {"type": "text", "text": "done"},
        ],
    });
    let out = wrap_response_signatures(Bytes::from(raw.to_string()), THIRD_PARTY);
    let value: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        unwrap_upstream_signature(value["content"][0]["signature"].as_str().unwrap()),
        Some((THIRD_PARTY.to_string(), "mm-sig".to_string()))
    );
    assert_eq!(
        value["content"][1]["signature"], "",
        "an empty signature is no host's and stays as it is"
    );
    assert_eq!(value["content"][2]["text"], "done");
}

#[test]
fn a_buffered_response_without_a_signature_keeps_its_bytes() {
    let raw = Bytes::from_static(br#"{ "content": [{"type":"text","text":"hi"}] }"#);
    assert_eq!(wrap_response_signatures(raw.clone(), THIRD_PARTY), raw);
}

// ---------------------------------------------------------------- streaming

type Item = Result<Bytes, std::convert::Infallible>;

fn chunk(text: &str) -> Item {
    Ok(Bytes::from(text.to_owned()))
}

async fn collect(items: Vec<Item>, key: Option<&str>) -> String {
    collect_within(items, key, 64 * 1024).await
}

async fn collect_within(items: Vec<Item>, key: Option<&str>, max_frame: usize) -> String {
    collect_bounded(items, key, max_frame, MAX_SIGNATURE_BYTES).await
}

async fn collect_bounded(
    items: Vec<Item>,
    key: Option<&str>,
    max_frame: usize,
    max_signature: usize,
) -> String {
    wrap_signature_stream(
        stream::iter(items),
        key.map(str::to_owned),
        max_frame,
        max_signature,
    )
    .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
    .collect()
    .await
}

const TEXT_DELTA: &str = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";

fn signature_deltas(out: &str) -> Vec<(u64, String)> {
    out.split("\n\n")
        .filter_map(super::frame_data)
        .filter(|event| event["delta"]["type"] == "signature_delta")
        .map(|event| {
            (
                event["index"].as_u64().unwrap(),
                event["delta"]["signature"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The shape a client concatenates: a start carrying part of the signature
/// and several `signature_delta` fragments, with frames and even the frame
/// boundary itself split across chunks.
#[tokio::test]
async fn a_streamed_signature_leaves_as_one_wrapped_delta_before_its_stop() {
    let out = collect(
        vec![
            chunk("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"AA\"}}\n\n"),
            chunk("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_"),
            chunk("delta\",\"signature\":\"BB\"}}\n"),
            chunk("\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"CC\"}}\n\n"),
            chunk("event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n"),
            chunk(TEXT_DELTA),
        ],
        Some(THIRD_PARTY),
    )
    .await;

    let deltas = signature_deltas(&out);
    assert_eq!(
        deltas.len(),
        1,
        "one signature_delta, not one per fragment: {out}"
    );
    assert_eq!(
        unwrap_upstream_signature(&deltas[0].1),
        Some((THIRD_PARTY.to_string(), "AABBCC".to_string())),
        "the start's part and every fragment, in order, under one envelope"
    );
    let delta_at = out.find("signature_delta").unwrap();
    let stop_at = out.find("content_block_stop").unwrap();
    assert!(
        delta_at < stop_at && !out[delta_at..stop_at].contains("content_block_delta\ndata"),
        "the wrapped delta sits immediately before the block's stop"
    );
    assert!(
        out.contains("\"signature\":\"\"}}"),
        "the start's own signature is blanked, so the client's concatenation is the envelope alone"
    );
    assert!(out.contains("\"thinking\":\"hmm\""));
    assert!(
        out.ends_with(TEXT_DELTA),
        "a text frame passes byte-identical"
    );
}

#[tokio::test]
async fn a_text_frame_is_forwarded_as_soon_as_it_completes() {
    // The upstream then goes quiet forever: the frame must not be held for
    // anything that follows it.
    let upstream = stream::iter(vec![chunk(TEXT_DELTA)]).chain(stream::pending());
    let mut wrapped = Box::pin(wrap_signature_stream(
        upstream,
        Some(THIRD_PARTY.to_string()),
        64 * 1024,
        MAX_SIGNATURE_BYTES,
    ));
    let first = wrapped
        .next()
        .now_or_never()
        .expect("the completed frame is ready without waiting on the upstream")
        .expect("an item")
        .unwrap();
    assert_eq!(first, Bytes::from_static(TEXT_DELTA.as_bytes()));
}

#[tokio::test]
async fn a_stream_without_thinking_is_byte_identical() {
    let frames = [
        "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"m\"}}\r\n\r\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        TEXT_DELTA,
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ];
    let joined = frames.concat();
    // Split mid-frame so the framing, not the chunking, is what gets checked.
    let (head, tail) = joined.split_at(40);
    assert_eq!(
        collect(vec![chunk(head), chunk(tail)], Some(THIRD_PARTY)).await,
        joined
    );
}

#[tokio::test]
async fn an_oversized_frame_is_forwarded_verbatim_and_framing_resumes() {
    let big = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\n\n",
        "x".repeat(256)
    );
    let (head, tail) = big.split_at(big.len() - 1);
    let start = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"SIG\"}}\n\n";
    let stop = "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n";
    // The boundary of the oversized frame straddles two chunks.
    let out = collect_within(
        vec![chunk(head), chunk(&format!("{tail}{start}{stop}"))],
        Some(THIRD_PARTY),
        64,
    )
    .await;
    assert!(
        out.starts_with(&big),
        "the oversized frame is forwarded as it stands"
    );
    assert_eq!(
        signature_deltas(&out).len(),
        1,
        "and the frames after its boundary are framed — and wrapped — again"
    );
}

#[tokio::test]
async fn an_oversized_frame_boundary_is_found_wherever_the_chunks_cut_it() {
    let big = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\r\n\r\n",
        "x".repeat(256)
    );
    let start = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"SIG\"}}\n\n";
    let stop = "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n";
    let rest = format!("{start}{stop}");
    let n = big.len();
    let cuts: [Vec<String>; 2] = [
        // The boundary lies deep inside a later chunk, past its edge.
        vec![big[..n - 50].into(), format!("{}{rest}", &big[n - 50..])],
        // The boundary arrives a byte or two at a time, after a long chunk.
        vec![
            big[..n - 40].into(),
            big[n - 40..n - 3].into(),
            big[n - 3..n - 2].into(),
            big[n - 2..n - 1].into(),
            format!("{}{rest}", &big[n - 1..]),
        ],
    ];
    for chunks in cuts {
        let out = collect_within(
            chunks.iter().map(|c| chunk(c)).collect(),
            Some(THIRD_PARTY),
            64,
        )
        .await;
        assert!(out.starts_with(&big), "the oversized frame passes verbatim");
        assert_eq!(
            signature_deltas(&out).len(),
            1,
            "and framing resumes at its boundary"
        );
    }
}

// ------------------------------------------------------- signature bounds

/// The ceiling composition, which no stream outcome distinguishes from an
/// inverted `min`: both arms of the regression still bound the stream.
#[test]
fn the_signature_ceiling_takes_whichever_bound_is_smaller() {
    assert_eq!(
        signature_ceiling(None),
        MAX_SIGNATURE_BYTES,
        "the client path is bounded by the constant alone"
    );
    assert_eq!(
        signature_ceiling(Some(1024)),
        1024,
        "a cap below the constant is the binding one"
    );
    assert_eq!(
        signature_ceiling(Some(MAX_SIGNATURE_BYTES * 2)),
        MAX_SIGNATURE_BYTES,
        "a cap above the constant must not raise the ceiling"
    );
}

/// Marks every raw signature in these tests, so a leak of any part of one is
/// a substring the output must not contain. Base64 envelopes cannot carry it.
const RAW: &str = "RAWSIG";

/// A `max_frame` every frame here fits under except a signature frame built
/// past it: a thinking start carries `"signature":""`, so a start that
/// outgrew the bound and was skipped would fail closed on its own.
const SMALL_FRAME: usize = 200;

fn raw_signature(len: usize) -> String {
    format!("{RAW}{}", "r".repeat(len))
}

fn thinking_start(index: u64) -> String {
    format!("event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}}}\n\n")
}

fn signature_delta(index: u64, signature: &str) -> String {
    format!("event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":{{\"type\":\"signature_delta\",\"signature\":\"{signature}\"}}}}\n\n")
}

fn block_stop(index: u64) -> String {
    format!("event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\n")
}

/// `text` cut into chunks of `size` bytes (ASCII, so every cut is a char
/// boundary).
fn chunks_of(text: &str, size: usize) -> Vec<Item> {
    text.as_bytes()
        .chunks(size)
        .map(|piece| chunk(std::str::from_utf8(piece).unwrap()))
        .collect()
}

/// The stream failed closed: no raw signature byte left, the last thing out is
/// the `error` event, it starts a frame of its own, and nothing follows it.
fn assert_failed_closed(out: &str) {
    assert!(!out.contains(RAW), "a raw signature leaked: {out}");
    let at = out
        .rfind("event: error\n")
        .unwrap_or_else(|| panic!("no error event: {out}"));
    assert!(
        at == 0 || out[..at].ends_with("\n\n"),
        "the error event is a frame of its own: {out}"
    );
    let error = &out[at..];
    assert_eq!(
        error.find("\n\n"),
        Some(error.len() - 2),
        "nothing follows the error event: {error}"
    );
    let event = frame_data(error).expect("the error event parses");
    assert_eq!(event["type"], "error");
    assert_eq!(event["error"]["type"], "api_error");
}

/// A signature frame past `max_frame` is held to its boundary rather than
/// forwarded half-read, so however the chunks cut it, it leaves wrapped.
/// Non-vacuity: forwarding it at `max_frame`, as an unsigned frame is, puts
/// the raw signature in the output.
#[tokio::test]
async fn an_oversized_signature_frame_is_held_and_leaves_wrapped() {
    let signature = raw_signature(300);
    let body = format!(
        "{}{}{}{TEXT_DELTA}",
        thinking_start(0),
        signature_delta(0, &signature),
        block_stop(0)
    );
    let out = collect_bounded(chunks_of(&body, 50), Some(THIRD_PARTY), SMALL_FRAME, 4096).await;
    assert!(!out.contains(RAW), "the raw signature never leaves: {out}");
    assert_eq!(
        signature_deltas(&out)
            .into_iter()
            .map(|(_, signature)| unwrap_upstream_signature(&signature))
            .collect::<Vec<_>>(),
        vec![Some((THIRD_PARTY.to_string(), signature))],
        "exactly one wrapped signature_delta"
    );
    assert!(out.ends_with(TEXT_DELTA), "and the stream carries on");
}

/// An upstream that ends inside a signature frame leaves that frame
/// incomplete; it is dropped rather than forwarded raw. A frame without a
/// signature still goes out as it stands.
/// Non-vacuity: forwarding every incomplete frame at the end, as unsigned
/// ones are, puts the raw signature in the output.
#[tokio::test]
async fn an_unfinished_signature_frame_at_the_end_is_dropped() {
    let unfinished = format!(
        "{}{}",
        thinking_start(0),
        signature_delta(0, &raw_signature(20)).trim_end()
    );
    let out = collect(vec![chunk(&unfinished)], Some(THIRD_PARTY)).await;
    assert_eq!(out, thinking_start(0), "only the complete start goes out");

    let unsigned = TEXT_DELTA.trim_end();
    let out = collect(vec![chunk(unsigned)], Some(THIRD_PARTY)).await;
    assert_eq!(out, unsigned, "an unsigned incomplete frame is kept");
}

/// A signature frame larger than the signature bound cannot be held, and
/// forwarding it would leak it: the stream fails closed instead.
#[tokio::test]
async fn a_signature_frame_past_the_signature_bound_fails_closed() {
    let body = format!(
        "{}{}{}{TEXT_DELTA}",
        thinking_start(0),
        signature_delta(0, &raw_signature(600)),
        block_stop(0)
    );
    let out = collect_bounded(chunks_of(&body, 50), Some(THIRD_PARTY), SMALL_FRAME, 256).await;
    assert_failed_closed(&out);
    // The held frame is refused as it crosses the bound, not when it
    // completes: here it never does, and the end of the stream would
    // otherwise forward the incomplete frame as it stands.
    let unfinished = format!(
        "{}{}",
        thinking_start(0),
        signature_delta(0, &raw_signature(600)).trim_end()
    );
    let out = collect_bounded(
        chunks_of(&unfinished, 50),
        Some(THIRD_PARTY),
        SMALL_FRAME,
        256,
    )
    .await;
    assert_failed_closed(&out);
}

/// Fragments each far below every bound still add up: the withheld total is
/// what the signature bound caps.
#[tokio::test]
async fn withheld_fragments_past_the_signature_bound_fail_closed() {
    let mut body = thinking_start(0);
    for _ in 0..20 {
        body.push_str(&signature_delta(0, &raw_signature(20)));
    }
    body.push_str(&block_stop(0));
    body.push_str(TEXT_DELTA);
    let out = collect_bounded(vec![chunk(&body)], Some(THIRD_PARTY), 64 * 1024, 256).await;
    assert_failed_closed(&out);
}

/// A thinking start whose `"signature"` key lies past `max_frame` is already
/// being forwarded unread when the key arrives; the key fails the stream
/// closed before its value goes out, wherever the chunks cut it.
#[tokio::test]
async fn a_signature_key_in_a_skipped_frame_fails_closed() {
    let start = format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"thinking\",\"thinking\":\"{}\",\"signature\":\"{}\"}}}}\n\n",
        "x".repeat(300),
        raw_signature(40)
    );
    let body = format!("{start}{}{TEXT_DELTA}", block_stop(0));
    let key_at = body.find("\"signature\"").unwrap();
    // The key deep in a later chunk, then straddling the edge after its
    // opening quote, mid-word, and with only its closing quote left.
    for cut in [key_at - 50, key_at + 1, key_at + 5, key_at + 10] {
        let out = collect_bounded(
            vec![
                chunk(&body[..100]),
                chunk(&body[100..cut]),
                chunk(&body[cut..]),
            ],
            Some(THIRD_PARTY),
            64,
            4096,
        )
        .await;
        assert!(
            out.starts_with(&body[..100]),
            "cut {cut}: the frame was being forwarded"
        );
        assert_failed_closed(&out);
        assert!(
            out.contains("past the gateway's frame bound"),
            "cut {cut}: {out}"
        );
    }
}

/// The key watch looks for a JSON key, not the word. A text delta carrying
/// prose, an escaped `\"signature\"`, and a text ending `\"signature` (whose
/// own closing quote completes `"signature"` after a backslash) passes
/// byte-identical and framing resumes. The mentions sit both in the bytes
/// scanned when the frame outgrows `max_frame` and in the skipped bytes, cut
/// at the chunk edge right after the backslash, mid-word, and with the
/// backslash carried as the tail's oldest byte.
#[tokio::test]
async fn a_text_frame_mentioning_signature_is_not_mistaken_for_one() {
    let text = format!(
        "{} the signature is \\\"signature\\\" {} and it ends \\\"signature",
        "x".repeat(40),
        "y".repeat(200)
    );
    let big = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
    );
    let escaped_at = big.rfind("\\\"signature").unwrap();
    let rest = format!(
        "{}{}{}",
        thinking_start(2),
        signature_delta(2, "SIG"),
        block_stop(2)
    );
    for cut in [escaped_at + 1, escaped_at + 6, escaped_at + 11] {
        let out = collect_within(
            vec![
                chunk(&big[..200]),
                chunk(&big[200..cut]),
                chunk(&format!("{}{rest}", &big[cut..])),
            ],
            Some(THIRD_PARTY),
            64,
        )
        .await;
        assert!(
            out.starts_with(&big),
            "cut {cut}: the text frame passes verbatim"
        );
        assert!(!out.contains("event: error"), "cut {cut}: {out}");
        assert_eq!(
            signature_deltas(&out).len(),
            1,
            "cut {cut}: framing resumes"
        );
    }
}

/// A string value reading `signature` is not a key. An oversized non-thinking
/// frame carrying `"mode":"signature"` passes byte-identical and framing
/// resumes, wherever the chunks cut it, including right before the value's
/// opening quote.
#[tokio::test]
async fn a_signature_string_value_is_not_mistaken_for_a_key() {
    let big = format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"n\",\"input\":{{\"q\":\"{}\",\"mode\":\"signature\"}}}}}}\n\n",
        "x".repeat(300)
    );
    let value_at = big.find(":\"signature\"").unwrap() + 1;
    let rest = format!(
        "{}{}{}",
        thinking_start(2),
        signature_delta(2, "SIG"),
        block_stop(2)
    );
    for cut in [value_at - 3, value_at, value_at + 1, value_at + 6] {
        let out = collect_within(
            vec![
                chunk(&big[..200]),
                chunk(&big[200..cut]),
                chunk(&format!("{}{rest}", &big[cut..])),
            ],
            Some(THIRD_PARTY),
            64,
        )
        .await;
        assert!(
            out.starts_with(&big),
            "cut {cut}: the tool_use frame passes verbatim"
        );
        assert!(!out.contains("event: error"), "cut {cut}: {out}");
        assert_eq!(
            signature_deltas(&out).len(),
            1,
            "cut {cut}: framing resumes"
        );
    }
}

/// A key right after a continuation line's `data:` is still a key: the colon
/// before it is the SSE field's, not a JSON one. Judged by what precedes it,
/// this oversized signature frame was skipped and its signature leaked raw.
#[tokio::test]
async fn a_signature_key_opening_a_data_continuation_is_held() {
    let signature = raw_signature(300);
    let split = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"signature_delta\",\ndata:\"signature\":\"{signature}\"}}}}\n\n"
    );
    let body = format!("{}{split}{}{TEXT_DELTA}", thinking_start(0), block_stop(0));
    let out = collect_bounded(chunks_of(&body, 50), Some(THIRD_PARTY), SMALL_FRAME, 4096).await;
    assert!(!out.contains(RAW), "the raw signature never leaves: {out}");
    assert_eq!(
        signature_deltas(&out)
            .into_iter()
            .map(|(_, signature)| unwrap_upstream_signature(&signature))
            .collect::<Vec<_>>(),
        vec![Some((THIRD_PARTY.to_string(), signature))],
        "held to its boundary and wrapped"
    );
}

/// A string value reading `signature` is no key whatever precedes it — `,` in
/// an array, a space after `:` — because no `:` follows it. Oversized
/// tool_use frames carrying one pass byte-identical, wherever the chunks cut
/// the match, including between its closing quote and the byte after it.
#[tokio::test]
async fn a_signature_value_after_a_comma_or_space_is_not_a_key() {
    for value in [r#""tags":["a","signature"]"#, r#""mode": "signature""#] {
        let big = format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"n\",\"input\":{{\"q\":\"{}\",{value}}}}}}}\n\n",
            "x".repeat(300)
        );
        let match_at = big.rfind("\"signature\"").unwrap();
        let rest = format!(
            "{}{}{}",
            thinking_start(2),
            signature_delta(2, "SIG"),
            block_stop(2)
        );
        for cut in [match_at, match_at + 5, match_at + 11, match_at + 12] {
            let out = collect_within(
                vec![
                    chunk(&big[..200]),
                    chunk(&big[200..cut]),
                    chunk(&format!("{}{rest}", &big[cut..])),
                ],
                Some(THIRD_PARTY),
                64,
            )
            .await;
            assert!(
                out.starts_with(&big),
                "{value} cut {cut}: the tool_use frame passes verbatim"
            );
            assert!(!out.contains("event: error"), "{value} cut {cut}: {out}");
            assert_eq!(
                signature_deltas(&out).len(),
                1,
                "{value} cut {cut}: framing resumes"
            );
        }
    }
}

/// A skipped frame's key with a space before its `:`, the `:` arriving only
/// with the next chunk: the match is carried and decided then, and fails the
/// stream closed before the value goes out.
#[tokio::test]
async fn a_skipped_key_whose_colon_arrives_next_chunk_fails_closed() {
    let start = format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"thinking\",\"thinking\":\"{}\",\"signature\" :\"{}\"}}}}\n\n",
        "x".repeat(300),
        raw_signature(40)
    );
    let body = format!("{start}{}{TEXT_DELTA}", block_stop(0));
    let key_at = body.find("\"signature\"").unwrap();
    // After the closing quote, and after the space: the `:` is next chunk's.
    for cut in [key_at + 11, key_at + 12] {
        let out = collect_bounded(
            vec![
                chunk(&body[..100]),
                chunk(&body[100..cut]),
                chunk(&body[cut..]),
            ],
            Some(THIRD_PARTY),
            64,
            4096,
        )
        .await;
        assert_failed_closed(&out);
        let error = &out[out.rfind("event: error\n").unwrap()..];
        assert_eq!(
            frame_data(error).unwrap()["error"]["message"],
            SKIPPED_KEY_MESSAGE,
            "cut {cut}"
        );
    }
}

/// An empty `signature_delta` withholds nothing, so it opens no entry: empty
/// deltas across many indices, never stopped, would otherwise grow the map
/// with nothing counted against `max_signature`. The map is inspected
/// directly because the output is the same either way. A real signature on
/// another index still wraps.
#[test]
fn an_empty_signature_fragment_opens_no_entry() {
    let mut wrap = SignatureWrap::new(THIRD_PARTY.to_string(), 64 * 1024, MAX_SIGNATURE_BYTES);
    let mut empties = String::new();
    for index in 0..50 {
        empties.push_str(&signature_delta(index, ""));
    }
    assert_eq!(wrap.push(empties.as_bytes()), Vec::<u8>::new());
    assert!(wrap.signatures.is_empty(), "{:?}", wrap.signatures.len());
    assert_eq!(wrap.withheld, 0);

    let real = format!(
        "{}{}{}",
        thinking_start(50),
        signature_delta(50, "SIG"),
        block_stop(50)
    );
    let out = String::from_utf8(wrap.push(real.as_bytes())).unwrap();
    assert_eq!(
        signature_deltas(&out)
            .into_iter()
            .map(|(index, signature)| (index, unwrap_upstream_signature(&signature)))
            .collect::<Vec<_>>(),
        vec![(50, Some((THIRD_PARTY.to_string(), "SIG".to_string())))]
    );
    // Stopping an index that withheld nothing leaves its stop untouched.
    assert_eq!(
        wrap.push(block_stop(0).as_bytes()),
        block_stop(0).into_bytes()
    );
}

/// Each open entry is charged against `max_signature`, not only its bytes:
/// one-byte fragments across distinct indices, never stopped, would otherwise
/// hold far more memory than the bound counts.
/// Non-vacuity: charging bytes alone counts these 20 fragments as 20 bytes,
/// far under the bound, and nothing fails.
#[tokio::test]
async fn open_signature_entries_are_charged_against_the_signature_bound() {
    let mut body = String::new();
    for index in 0..20 {
        body.push_str(&signature_delta(index, "A"));
    }
    let out = collect_bounded(vec![chunk(&body)], Some(THIRD_PARTY), 64 * 1024, 1024).await;
    assert_failed_closed(&out);
}

/// The twin: a block's charge is released when its signature is emitted, and
/// it is charged once per block, not per fragment. The same 20 blocks under
/// the same bound, each stopped before the next opens and fed in several
/// fragments, all wrap.
/// Non-vacuity: keeping the charge after the stop, or charging every
/// fragment, takes the total past 1024 and fails the stream closed.
#[tokio::test]
async fn a_stopped_block_releases_its_signature_entry_charge() {
    let mut body = String::new();
    for index in 0..20 {
        body.push_str(&thinking_start(index));
        for _ in 0..20 {
            body.push_str(&signature_delta(index, "A"));
        }
        body.push_str(&block_stop(index));
    }
    let out = collect_bounded(vec![chunk(&body)], Some(THIRD_PARTY), 64 * 1024, 1024).await;
    assert!(!out.contains("event: error"), "nothing fails: {out}");
    assert_eq!(
        signature_deltas(&out)
            .into_iter()
            .map(|(index, signature)| (index, unwrap_upstream_signature(&signature)))
            .collect::<Vec<_>>(),
        (0..20)
            .map(|index| (index, Some((THIRD_PARTY.to_string(), "A".repeat(20)))))
            .collect::<Vec<_>>()
    );
}

/// A frame held only because its match was undecided as it crossed
/// `max_frame` is released once the next bytes show the match to be a value:
/// forwarded and skipped like any unsigned frame, not held to its boundary
/// and failed closed when it outgrows `max_signature`.
#[tokio::test]
async fn a_provisional_hold_is_released_when_the_match_proves_a_value() {
    let big = format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"n\",\"input\":{{\"q\":\"{}\",\"mode\":\"signature\",\"more\":\"{}\"}}}}}}\n\n",
        "x".repeat(50),
        "y".repeat(400)
    );
    let match_at = big.find("\"signature\"").unwrap();
    let rest = format!(
        "{}{}{}",
        thinking_start(2),
        signature_delta(2, "SIG"),
        block_stop(2)
    );
    // The cut leaves the first chunk under `max_signature`, so only a hold
    // kept past the match being decided can reach it.
    assert!(match_at + 13 < 256);
    let n = big.len();
    // Right after the value's closing quote (undecided), and two bytes on.
    for cut in [match_at + 11, match_at + 13] {
        let out = collect_bounded(
            vec![
                chunk(&big[..cut]),
                // Past `max_signature`, still short of the boundary.
                chunk(&big[cut..n - 2]),
                chunk(&format!("{}{rest}", &big[n - 2..])),
            ],
            Some(THIRD_PARTY),
            64,
            256,
        )
        .await;
        assert!(
            out.starts_with(&big),
            "cut {cut}: the tool_use frame passes verbatim"
        );
        assert!(!out.contains("event: error"), "cut {cut}: {out}");
        assert_eq!(
            signature_deltas(&out).len(),
            1,
            "cut {cut}: framing resumes"
        );
    }
}

/// An event whose JSON spans two `data:` lines is valid SSE; read line by
/// line its signature would not parse and would pass through raw.
#[tokio::test]
async fn a_signature_delta_split_over_data_lines_is_wrapped() {
    let split = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\ndata: \"delta\":{{\"type\":\"signature_delta\",\"signature\":\"{RAW}\"}}}}\n\n"
    );
    let body = format!("{}{split}{}", thinking_start(0), block_stop(0));
    let out = collect(vec![chunk(&body)], Some(THIRD_PARTY)).await;
    assert!(!out.contains(RAW), "{out}");
    assert_eq!(
        signature_deltas(&out)
            .into_iter()
            .map(|(_, signature)| unwrap_upstream_signature(&signature))
            .collect::<Vec<_>>(),
        vec![Some((THIRD_PARTY.to_string(), RAW.to_string()))]
    );
}

// ------------------------------------------------------------ relay_response

fn relay_state() -> crate::server::AppState {
    let mut config = Config::default();
    config.providers.insert(
        "minimax".into(),
        provider(THIRD_PARTY, AuthMode::Passthrough),
    );
    config.providers.insert(
        "claude".into(),
        provider("http://127.0.0.1:9", AuthMode::ClaudeOauth),
    );
    crate::server::AppState::new(config, reqwest::Client::new()).unwrap()
}

fn route(provider: &str) -> crate::routing::Route {
    crate::routing::Route {
        provider: provider.to_string(),
        adapter: crate::routing::AdapterKind::Anthropic,
        model: "m".to_string(),
        upstream_model: "m".to_string(),
        effort: None,
        service_tier: None,
    }
}

async fn relayed(provider: &str, content_type: &str, body: &str) -> String {
    // An empty `content_type` sends the reply with no content-type at all.
    let mut builder = axum::http::Response::builder().status(200);
    if !content_type.is_empty() {
        builder = builder.header("content-type", content_type);
    }
    let upstream =
        reqwest::Response::from(builder.body(reqwest::Body::from(body.to_owned())).unwrap());
    let (_, response) = super::super::relay_response(
        &relay_state(),
        &route(provider),
        upstream,
        None,
        crate::adapters::ResponseBounds {
            max_bytes: None,
            idle: None,
        },
    )
    .await
    .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The twin pair at the hook every relay goes through: the same body is
/// wrapped for the third-party provider and left byte-identical for
/// api.anthropic.com and a `claude_oauth` pool, streamed and buffered alike.
#[tokio::test]
async fn relay_response_wraps_third_party_signatures_only() {
    let json_body = r#"{"content":[{"type":"thinking","thinking":"t","signature":"orig"}]}"#;
    let sse_body = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"orig\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n";

    let json_out = relayed("minimax", "application/json", json_body).await;
    let value: Value = serde_json::from_str(&json_out).unwrap();
    assert_eq!(
        unwrap_upstream_signature(value["content"][0]["signature"].as_str().unwrap()),
        Some((key_of(THIRD_PARTY), "orig".to_string()))
    );
    // The media type is matched case-insensitively, and an untyped reply is
    // wrapped too: either left alone would carry the raw signature out.
    for content_type in [
        "Application/JSON; charset=utf-8",
        "application/vnd.api+json",
        "",
    ] {
        let value: Value =
            serde_json::from_str(&relayed("minimax", content_type, json_body).await).unwrap();
        assert_eq!(
            unwrap_upstream_signature(value["content"][0]["signature"].as_str().unwrap()),
            Some((key_of(THIRD_PARTY), "orig".to_string())),
            "{content_type:?}: a buffered body is wrapped"
        );
    }
    let sse_out = relayed("minimax", "text/event-stream", sse_body).await;
    assert_eq!(
        signature_deltas(&sse_out)
            .into_iter()
            .map(|(_, signature)| unwrap_upstream_signature(&signature))
            .collect::<Vec<_>>(),
        vec![Some((key_of(THIRD_PARTY), "orig".to_string()))]
    );

    for provider in ["anthropic", "claude"] {
        assert_eq!(
            relayed(provider, "application/json", json_body).await,
            json_body,
            "{provider}: a buffered body stays passthrough"
        );
        assert_eq!(
            relayed(provider, "text/event-stream", sse_body).await,
            sse_body,
            "{provider}: a stream stays passthrough"
        );
    }
}

// --------------------------------------------------------------- full loop

async fn last_received(server: &wiremock::MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    serde_json::from_slice(&requests.last().unwrap().body).unwrap()
}

/// The `Accept-Encoding` the upstream last received.
async fn last_accept_encoding(server: &wiremock::MockServer) -> Option<String> {
    let requests = server.received_requests().await.unwrap();
    let headers = &requests.last().unwrap().headers;
    headers
        .get("accept-encoding")
        .map(|value| value.to_str().unwrap().to_string())
}

/// The whole round trip through `forward`, against wiremock upstreams: a
/// third-party response leaves wrapped, the wrapped transcript goes back to
/// that provider carrying the original signature, and the same transcript sent
/// to a `claude_oauth` pool arrives with the thinking block removed.
#[tokio::test]
async fn a_third_party_signature_round_trips_home_and_never_reaches_the_claude_pool() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let minimax = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [
                {"type": "thinking", "thinking": "weighing it", "signature": "mm-original"},
                {"type": "text", "text": "done"},
            ],
        })))
        .mount(&minimax)
        .await;
    let claude = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"content": []})))
        .mount(&claude)
        .await;

    let credentials = std::env::temp_dir().join(format!(
        "shunt-thinking-wrap-{}-claude.json",
        std::process::id()
    ));
    std::fs::write(
        &credentials,
        r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":4000000000000}}"#,
    )
    .unwrap();
    let mut config = Config::default();
    // Passthrough: the client's own header is the credential, so nothing reads
    // the environment.
    config.providers.insert(
        "minimax".into(),
        provider(&minimax.uri(), AuthMode::Passthrough),
    );
    let mut pool = provider(&claude.uri(), AuthMode::ClaudeOauth);
    pool.accounts = vec![crate::config::AccountConfig {
        name: "acct".to_string(),
        credentials: Some(credentials.display().to_string()),
        uuid: Some("uuid-acct".to_string()),
        ..Default::default()
    }];
    config.providers.insert("claude".into(), pool);
    let state = crate::server::AppState::new(config, reqwest::Client::new()).unwrap();

    let uri: axum::http::Uri = "/v1/messages".parse().unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-api-key", "client-key".parse().unwrap());
    headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
    headers.insert("accept-encoding", "gzip, br".parse().unwrap());
    let bounds = || crate::adapters::ResponseBounds {
        max_bytes: None,
        idle: None,
    };
    let send = |provider: &'static str, request: Value| {
        let state = state.clone();
        let uri = uri.clone();
        let headers = headers.clone();
        async move {
            let (_, response) = super::super::forward(
                state,
                route(provider),
                &uri,
                &headers,
                body(request),
                bounds(),
            )
            .await
            .unwrap_or_else(|_| panic!("{provider} relays"));
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
        }
    };

    // 1. The third-party response reaches the client wrapped.
    let first: Value = serde_json::from_slice(
        &send(
            "minimax",
            json!({"model": "m", "messages": [{"role": "user", "content": "go"}]}),
        )
        .await,
    )
    .unwrap();
    let wrapped = first["content"][0]["signature"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        unwrap_upstream_signature(&wrapped),
        Some((key_of(&minimax.uri()), "mm-original".to_string()))
    );
    // shunt decodes no content coding, so a compressed reply could not be
    // wrapped: the third-party host is asked for an uncompressed one.
    assert_eq!(
        last_accept_encoding(&minimax).await.as_deref(),
        Some("identity")
    );

    let next_turn = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": first["content"]},
            {"role": "user", "content": "and then?"},
        ],
    });

    // 2. Back to the same provider: the original signature, byte for byte.
    send("minimax", next_turn.clone()).await;
    let home = last_received(&minimax).await;
    assert_eq!(
        home["messages"][1]["content"][0]["signature"], "mm-original",
        "the issuing host gets its own signature back"
    );

    // 3. To the Claude pool: the block is gone, the rest of the turn intact.
    send("claude", next_turn).await;
    let away = last_received(&claude).await;
    let _ = std::fs::remove_file(&credentials);
    assert_eq!(
        last_accept_encoding(&claude).await.as_deref(),
        Some("gzip, br"),
        "a first-party request keeps the client's header"
    );
    assert_eq!(
        away["messages"][1]["content"],
        json!([{"type": "text", "text": "done"}]),
        "a signature Anthropic cannot verify never reaches it"
    );
}

// ------------------------------------------- chain_attempt / forward_kimi_oauth

fn chain_state(provider_name: &str, provider: ProviderConfig) -> crate::server::AppState {
    let mut config = Config::default();
    config.providers.insert(provider_name.into(), provider);
    crate::server::AppState::new(config, reqwest::Client::new()).unwrap()
}

fn client_headers() -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-api-key", "client-key".parse().unwrap());
    headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
    headers.insert("accept-encoding", "gzip, br".parse().unwrap());
    headers
}

async fn mock_messages(content_type: &str, body: &str) -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_owned(), content_type))
        .mount(&server)
        .await;
    server
}

/// A two-block transcript: the first assistant block is `signature`, the second
/// a genuine Anthropic one, so a strip that drops too much shows up too.
fn mixed_transcript(signature: &str) -> RequestBody {
    body(json!({
        "model": "m",
        "stream": true,
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "a", "signature": signature},
                {"type": "thinking", "thinking": "b", "signature": "genuine"},
                {"type": "text", "text": "done"},
            ]},
        ],
    }))
}

/// The request the failover chain's Anthropic entry sends upstream: foreign
/// envelopes dropped, genuine signatures intact, this provider's own envelope
/// restored. Deleting `prepare_thinking` from `chain_attempt` turns both halves
/// red.
#[tokio::test]
async fn chain_attempt_strips_foreign_and_restores_own_signatures() {
    let upstream = mock_messages("text/event-stream", "").await;
    let base = upstream.uri();
    let state = chain_state("minimax", provider(&base, AuthMode::Passthrough));
    let uri: axum::http::Uri = "/v1/messages".parse().unwrap();

    let foreign = wrap_upstream_signature(&key_of("https://other.example"), "other-sig");
    let attempt = super::super::chain_attempt(
        &state,
        &route("minimax"),
        &uri,
        &client_headers(),
        mixed_transcript(&foreign),
    )
    .await;
    assert!(matches!(
        attempt,
        crate::proxy::chain_stream::Attempt::Winner { .. }
    ));
    let content = last_received(&upstream).await["messages"][1]["content"].clone();
    assert_eq!(
        content,
        json!([
            {"type": "thinking", "thinking": "b", "signature": "genuine"},
            {"type": "text", "text": "done"},
        ]),
        "the foreign block is dropped and the genuine one kept"
    );

    let own = wrap_upstream_signature(&key_of(&base), "mm-original");
    super::super::chain_attempt(
        &state,
        &route("minimax"),
        &uri,
        &client_headers(),
        mixed_transcript(&own),
    )
    .await;
    let content = last_received(&upstream).await["messages"][1]["content"].clone();
    assert_eq!(content[0]["signature"], "mm-original");
    assert_eq!(content[1]["signature"], "genuine");
    assert_eq!(
        last_accept_encoding(&upstream).await.as_deref(),
        Some("identity")
    );
}

/// The streamed response of a chain attempt leaves wrapped for a third-party
/// winner. Deleting the `wrap_signature_stream` layering turns this red; the
/// first-party side of the pair is `relay_response_wraps_third_party_signatures_only`
/// (`chain_attempt` cannot reach a `claude_oauth` pool without its credential store).
#[tokio::test]
async fn chain_attempt_wraps_a_third_party_winning_stream() {
    let sse = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"orig\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
    let uri: axum::http::Uri = "/v1/messages".parse().unwrap();

    let streamed = |state: crate::server::AppState, name: &'static str| {
        let uri = uri.clone();
        async move {
            let attempt = super::super::chain_attempt(
                &state,
                &route(name),
                &uri,
                &client_headers(),
                mixed_transcript("genuine"),
            )
            .await;
            let crate::proxy::chain_stream::Attempt::Winner {
                relay: crate::proxy::chain_stream::RelayBuild::Ready { frames, .. },
                ..
            } = attempt
            else {
                panic!("expected a ready winner");
            };
            frames
                .map(|frame| String::from_utf8(frame.unwrap().to_vec()).unwrap())
                .collect::<String>()
                .await
        }
    };

    let upstream = mock_messages("text/event-stream", sse).await;
    let out = streamed(
        chain_state("minimax", provider(&upstream.uri(), AuthMode::Passthrough)),
        "minimax",
    )
    .await;
    assert_eq!(
        signature_deltas(&out)
            .into_iter()
            .map(|(_, signature)| unwrap_upstream_signature(&signature))
            .collect::<Vec<_>>(),
        vec![Some((key_of(&upstream.uri()), "orig".to_string()))],
        "{out}"
    );
}

/// `forward_kimi_oauth` is a third-party signing host too: its own envelope
/// comes back as the original signature, a foreign one is dropped. The
/// account reads an unexpired token from its own credential file, so the test
/// needs no network and mutates no process environment, which a concurrent
/// lib test reading an unrelated variable could otherwise observe (issue #788).
#[tokio::test]
async fn kimi_oauth_restores_its_own_envelope_and_drops_a_foreign_one() {
    let _env_lock = crate::config::test_env_lock();
    let upstream = mock_messages("application/json", r#"{"content":[]}"#).await;
    let base = upstream.uri();
    let credentials = std::env::temp_dir().join(format!(
        "shunt-thinking-wrap-{}-kimi.json",
        std::process::id()
    ));
    std::fs::write(
        &credentials,
        r#"{"kimiOauth":{"accessToken":"kimi-token","expiresAt":4000000000000},"deviceId":"test-device"}"#,
    )
    .unwrap();
    let mut kimi = provider(&base, AuthMode::KimiOauth);
    kimi.accounts = vec![crate::config::AccountConfig {
        name: "acct".to_string(),
        credentials: Some(credentials.display().to_string()),
        ..Default::default()
    }];
    let state = chain_state("kimi", kimi);
    let uri: axum::http::Uri = "/v1/messages".parse().unwrap();
    let send = |request: RequestBody| {
        let state = state.clone();
        let uri = uri.clone();
        async move {
            super::super::forward(
                state,
                route("kimi"),
                &uri,
                &client_headers(),
                request,
                crate::adapters::ResponseBounds {
                    max_bytes: None,
                    idle: None,
                },
            )
            .await
            .unwrap_or_else(|_| panic!("kimi relays"));
        }
    };

    send(mixed_transcript(&wrap_upstream_signature(
        &key_of(&base),
        "kimi-original",
    )))
    .await;
    let content = last_received(&upstream).await["messages"][1]["content"].clone();
    assert_eq!(content[0]["signature"], "kimi-original");
    assert_eq!(content[1]["signature"], "genuine");
    assert_eq!(
        last_accept_encoding(&upstream).await.as_deref(),
        Some("identity")
    );

    let foreign = wrap_upstream_signature(&key_of("https://other.example"), "other-sig");
    send(mixed_transcript(&foreign)).await;
    let content = last_received(&upstream).await["messages"][1]["content"].clone();
    let _ = std::fs::remove_file(&credentials);
    assert_eq!(content[0]["signature"], "genuine");
    assert_eq!(content[1]["type"], "text");
}
