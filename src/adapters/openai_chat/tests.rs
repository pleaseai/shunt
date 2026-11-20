use axum::http::StatusCode;

use super::sse::Decoder;
use super::{map_openai_chat_error, unary_cap, MAX_OPENAI_CHAT_UNARY_RESPONSE_BYTES};

#[test]
fn unary_cap_keeps_the_call_cap_and_defaults_to_the_adapter_ceiling() {
    let bounds = |max_bytes| crate::adapters::ResponseBounds {
        max_bytes,
        idle: None,
    };
    assert_eq!(
        unary_cap(bounds(None)),
        Some(MAX_OPENAI_CHAT_UNARY_RESPONSE_BYTES)
    );
    assert_eq!(unary_cap(bounds(Some(1024))), Some(1024));
    let above = MAX_OPENAI_CHAT_UNARY_RESPONSE_BYTES + 1;
    assert_eq!(unary_cap(bounds(Some(above))), Some(above));
}

#[test]
fn parsed_and_raw_error_messages_are_bounded() {
    let huge = "x".repeat(8_000);
    let json = format!(r#"{{"error":{{"message":"{huge}"}}}}"#);
    let parsed = map_openai_chat_error(StatusCode::BAD_GATEWAY, &json);
    assert!(
        parsed.message.chars().count() <= 4097,
        "{}",
        parsed.message.len()
    );
    assert!(parsed.message.ends_with('…'));

    let raw = map_openai_chat_error(StatusCode::BAD_GATEWAY, &huge);
    assert!(raw.message.ends_with('…'));
    assert!(raw.message.len() <= 4096 + '…'.len_utf8());
}

#[test]
fn comment_after_done_is_ignored_and_payload_is_not() {
    let mut decoder = Decoder::default();
    let first = b"data: [DONE]\n\n: keep-alive\n\n";
    let (consumed, item) = decoder.push_one(first).unwrap();
    assert!(matches!(item, Some(super::sse::Item::Done)));
    let (extra, item) = decoder.push_one(&first[consumed..]).unwrap();
    assert!(item.is_none(), "a comment carries no post-terminal payload");
    assert_eq!(consumed + extra, first.len());

    let mut decoder = Decoder::default();
    let duplicate = b"data: [DONE]\n\ndata: [DONE]\n\n";
    let (consumed, _) = decoder.push_one(duplicate).unwrap();
    let error = decoder.push_one(&duplicate[consumed..]).unwrap_err();
    assert!(error.contains("after [DONE]"), "{error}");
}

#[test]
fn ignorable_buffer_is_only_whitespace() {
    let mut decoder = Decoder::default();
    let (_, item) = decoder.push_one(b"   ").unwrap();
    assert!(item.is_none());
    assert!(decoder.buffered_is_ignorable());
    decoder.push_one(b"data").unwrap();
    assert!(!decoder.buffered_is_ignorable());
}
