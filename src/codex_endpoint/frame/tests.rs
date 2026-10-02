use super::*;

#[test]
fn parse_response_create_frame() {
    let json_str = r#"{"type":"response.create","generate":false,"model":"gpt-5.4-mini"}"#;
    let msg = Message::Text(json_str.into());
    let frame = parse_client_frame(&msg);
    match frame {
        ClientFrame::ResponseCreate {
            generate, model, ..
        } => {
            assert!(!generate);
            assert_eq!(model.as_deref(), Some("gpt-5.4-mini"));
        }
        other => panic!("expected ResponseCreate, got {:?}", other),
    }
}

#[test]
fn parse_response_processed_is_noop() {
    let msg = Message::Text(r#"{"type":"response.processed"}"#.into());
    assert_eq!(parse_client_frame(&msg), ClientFrame::ResponseProcessed);
}

#[test]
fn parse_unparseable_or_unknown_text_frame_is_ignored() {
    let msg1 = Message::Text("not json".into());
    assert_eq!(parse_client_frame(&msg1), ClientFrame::IgnoredText);

    let msg2 = Message::Text(r#"{"type":"custom.unknown"}"#.into());
    assert_eq!(parse_client_frame(&msg2), ClientFrame::IgnoredText);
}

#[test]
fn parse_binary_frame_unsupported() {
    let msg = Message::Binary(vec![1, 2, 3].into());
    assert_eq!(parse_client_frame(&msg), ClientFrame::BinaryUnsupported);
}

#[test]
fn sse_framer_accepts_cr_only_blank_lines() {
    let mut framer = BoundedSseFrameBuffer::new(64);
    assert_eq!(
        framer.feed(b"data: first\r\r").unwrap(),
        vec![b"data: first".to_vec()]
    );
    assert_eq!(
        framer.feed(b"data: second\r\r").unwrap(),
        vec![b"data: second".to_vec()]
    );
}

#[test]
fn warmup_frames_have_expected_shape_and_empty_id() {
    let frames = build_warmup_completion_frames(Some("gpt-5.4-mini"));
    let created: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
    let completed: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();

    assert_eq!(created["type"], "response.created");
    assert_eq!(created["sequence_number"], 0);
    assert_eq!(created["response"]["id"], "");
    assert_eq!(created["response"]["status"], "in_progress");
    assert_eq!(created["response"]["model"], "gpt-5.4-mini");

    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["sequence_number"], 1);
    assert_eq!(completed["response"]["id"], "");
    assert_eq!(completed["response"]["status"], "completed");
    assert_eq!(completed["response"]["model"], "gpt-5.4-mini");

    assert_eq!(
        created["response"]["created_at"],
        completed["response"]["created_at"]
    );
}

#[test]
fn safe_response_headers_strip_secrets_and_keep_responses_metadata() {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("set-cookie", "session=secret"),
        ("authorization", "Bearer secret"),
        ("cookie", "client=secret"),
        ("x-shunt-internal", "private"),
        ("retry-after", "7"),
        ("x-request-id", "req-1"),
        ("openai-request-id", "openai-1"),
        ("x-codex-turn-state", "state-1"),
        ("openai-model", "gpt-5.4-mini"),
        ("x-models-etag", "etag-1"),
        ("x-reasoning-included", "true"),
        ("x-ratelimit-limit-requests", "100"),
        ("x-codex-primary-used-percent", "25"),
    ] {
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }

    let safe = safe_response_headers(&headers);
    for forbidden in ["set-cookie", "authorization", "cookie", "x-shunt-internal"] {
        assert!(!safe.contains_key(forbidden), "leaked {forbidden}");
    }
    for allowed in [
        "retry-after",
        "x-request-id",
        "openai-request-id",
        "x-codex-turn-state",
        "openai-model",
        "x-models-etag",
        "x-reasoning-included",
        "x-ratelimit-limit-requests",
        "x-codex-primary-used-percent",
    ] {
        assert!(safe.contains_key(allowed), "dropped {allowed}");
    }
}

#[test]
fn websocket_error_frame_has_responses_shape() {
    let frame = build_ws_error_frame(
        502,
        "protocol_error",
        "websocket_protocol_error",
        "upstream ended early",
        None,
    );
    let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(value["type"], "error");
    assert_eq!(value["status"], 502);
    assert_eq!(value["error"]["type"], "protocol_error");
    assert_eq!(value["error"]["code"], "websocket_protocol_error");
    assert_eq!(value["error"]["message"], "upstream ended early");
    assert_eq!(value["headers"], serde_json::json!({}));
}

pub mod sse_framer {
    use super::*;

    #[test]
    fn splits_chunks_and_reassembles_blocks() {
        let mut framer = BoundedSseFrameBuffer::new(MAX_CLIENT_SSE_FRAME_BYTES);
        let chunk1 = b"data: hello\n\n";
        let res1 = framer.feed(chunk1).unwrap();
        assert_eq!(res1.len(), 1);
        assert_eq!(
            parse_sse_block(std::str::from_utf8(&res1[0]).unwrap()),
            Some("hello".to_string())
        );

        // Split across chunks
        let chunk2a = b"data: first line\ndata: second";
        let chunk2b = b" line\n\n";
        let res2a = framer.feed(chunk2a).unwrap();
        assert_eq!(res2a.len(), 0);
        let res2b = framer.feed(chunk2b).unwrap();
        assert_eq!(res2b.len(), 1);
        assert_eq!(
            parse_sse_block(std::str::from_utf8(&res2b[0]).unwrap()),
            Some("first line\nsecond line".to_string())
        );
    }

    #[test]
    fn handles_crlf_and_mixed_delimiters() {
        for delim in [b"\n\n".as_slice(), b"\r\n\r\n", b"\n\r\n", b"\r\n\n"] {
            let mut framer = BoundedSseFrameBuffer::new(MAX_CLIENT_SSE_FRAME_BYTES);
            let mut input = b"data: test".to_vec();
            input.extend_from_slice(delim);
            let res = framer.feed(&input).unwrap();
            assert_eq!(res.len(), 1);
            assert_eq!(
                parse_sse_block(std::str::from_utf8(&res[0]).unwrap()),
                Some("test".to_string())
            );
        }
    }

    /// Feed `input` split at `split`, then finish, returning each block's payload.
    fn payloads_split_at(input: &[u8], split: usize) -> Vec<String> {
        let mut framer = BoundedSseFrameBuffer::new(MAX_CLIENT_SSE_FRAME_BYTES);
        let mut blocks = framer.feed(&input[..split]).unwrap();
        blocks.extend(framer.feed(&input[split..]).unwrap());
        blocks.extend(framer.finish().unwrap());
        blocks
            .iter()
            .filter_map(|block| parse_sse_block(std::str::from_utf8(block).unwrap()))
            .collect()
    }

    #[test]
    fn bare_cr_terminates_blank_line_after_lf_or_crlf() {
        for delim in [b"\n\r".as_slice(), b"\r\n\r"] {
            let mut input = b"data: a".to_vec();
            input.extend_from_slice(delim);
            input.extend_from_slice(b"data: b");
            input.extend_from_slice(delim);
            for split in 0..=input.len() {
                assert_eq!(
                    payloads_split_at(&input, split),
                    vec!["a".to_string(), "b".to_string()],
                    "delimiter {delim:?} split at {split}"
                );
            }
        }
    }

    #[test]
    fn bare_cr_delimiter_is_emitted_once_the_next_byte_arrives() {
        for delim in [b"\n\r".as_slice(), b"\r\n\r"] {
            let mut framer = BoundedSseFrameBuffer::new(MAX_CLIENT_SSE_FRAME_BYTES);
            let mut input = b"data: a".to_vec();
            input.extend_from_slice(delim);
            // The trailing CR may still become CRLF, so the block waits.
            assert!(framer.feed(&input).unwrap().is_empty(), "{delim:?}");
            assert_eq!(
                framer.feed(b"data: b").unwrap(),
                vec![b"data: a".to_vec()],
                "{delim:?}"
            );
            assert_eq!(framer.finish().unwrap(), Some(b"data: b".to_vec()));
        }
    }

    #[test]
    fn enforces_max_frame_bytes() {
        let mut framer = BoundedSseFrameBuffer::new(100);
        let oversized = vec![b'a'; 150];
        let err = framer.feed(&oversized).unwrap_err();
        assert_eq!(err, SseFrameError::TooLarge(100));
    }

    #[test]
    fn accepts_exact_cap_and_split_delimiter() {
        let mut framer = BoundedSseFrameBuffer::new(8);
        assert!(framer.feed(b"12345678\n").unwrap().is_empty());
        assert_eq!(framer.feed(b"\n").unwrap(), vec![b"12345678".to_vec()]);
        assert_eq!(framer.finish().unwrap(), None);
    }

    #[test]
    fn finish_returns_unterminated_trailing_block() {
        let mut framer = BoundedSseFrameBuffer::new(64);
        assert!(framer.feed(b"data: trailing").unwrap().is_empty());
        assert_eq!(framer.finish().unwrap(), Some(b"data: trailing".to_vec()));
    }

    #[test]
    fn limits_delimiter_only_frame_amplification() {
        let mut framer = BoundedSseFrameBuffer::new(4096);
        let err = framer.feed(b"\n\n\n\n\n\n\n\n\n\n").unwrap_err();
        assert_eq!(err, SseFrameError::CountLimit(4));
        assert_eq!(framer.finish().unwrap(), None);
    }

    #[test]
    fn preserves_terminal_frame_before_trailing_overflow() {
        let terminal = b"data: {\"type\":\"response.completed\"}\n\n";
        let mut input = terminal.to_vec();
        input.extend(std::iter::repeat_n(b'x', 65));
        let mut framer = BoundedSseFrameBuffer::new(64);
        let frames = framer.feed(&input).unwrap();
        assert_eq!(
            frames,
            vec![b"data: {\"type\":\"response.completed\"}".to_vec()]
        );
        assert_eq!(framer.finish().unwrap(), None);
    }

    #[test]
    fn classifies_terminal_and_ignores_done() {
        assert_eq!(parse_payload_type("[DONE]"), None);
        assert_eq!(
            parse_payload_type(r#"{"type":"response.completed"}"#),
            Some("response.completed".to_string())
        );
        assert_eq!(
            terminal_status_from_type("response.completed"),
            Some(TerminalStatus::Completed)
        );
        assert_eq!(
            terminal_status_from_type("response.failed"),
            Some(TerminalStatus::Failed)
        );
        assert_eq!(
            terminal_status_from_type("response.incomplete"),
            Some(TerminalStatus::Incomplete)
        );
        assert_eq!(
            terminal_status_from_type("error"),
            Some(TerminalStatus::Failed)
        );
        assert_eq!(terminal_status_from_type("response.created"), None);
    }
}
