//! The two terminal markers a retained turn is held to (ADR-0005 §3).
//!
//! `tests/buffer_replay.rs` drives them end to end through a truncated stream;
//! these pin the readings that a live test would only observe as "served" or
//! "cut".
//!
//! Non-vacuity: make [`TerminalScan::feed`] match `message_stop` anywhere in
//! the chunk rather than on a frame's `event:` line and
//! `a_message_stop_mentioned_in_content_is_not_the_marker` goes red; drop the
//! carried remainder and `a_message_stop_split_across_chunks_is_still_seen`
//! goes red; drop the `errored` check and `an_error_frame_is_never_terminal`
//! goes red; accept any JSON object in [`is_single_message`] and
//! `a_json_error_body_is_not_a_message` goes red; let any line ending, not
//! only one that closes a blank line, end the kept prefix in
//! [`complete_frames_len`], or stop holding back its trailing CR, and
//! `a_partial_frame_after_the_marker_is_cut_from_the_kept_bytes` goes red.

use super::{complete_frames_len, is_single_message, TerminalScan};

const START: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
const DELTA: &str =
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n";
const STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

fn scan(chunks: &[&str]) -> TerminalScan {
    let mut scan = TerminalScan::default();
    for chunk in chunks {
        scan.feed(chunk.as_bytes());
    }
    scan
}

#[test]
fn a_stream_ending_in_message_stop_is_terminal() {
    assert!(scan(&[START, DELTA, STOP]).is_terminal());
}

/// The truncated `200` the gate exists for: content, then the connection
/// simply ends.
#[test]
fn a_stream_without_message_stop_is_not_terminal() {
    assert!(!scan(&[START, DELTA]).is_terminal());
}

/// Under CRLF a frame ends in `\r\n\r\n`, which contains no `\n\n`.
#[test]
fn a_crlf_stream_is_read_the_same() {
    let crlf = |frame: &str| frame.replace('\n', "\r\n");
    assert!(scan(&[&crlf(START), &crlf(DELTA), &crlf(STOP)]).is_terminal());
}

#[test]
fn a_message_stop_split_across_chunks_is_still_seen() {
    let (head, tail) = STOP.split_at(9);
    assert!(scan(&[START, DELTA, head, tail]).is_terminal());
    // And the half alone is not the marker: the frame is not complete.
    assert!(!scan(&[START, DELTA, head]).is_terminal());
}

/// A frame whose terminator never arrived is not a complete frame, even when
/// its `event:` line is whole.
#[test]
fn an_unterminated_message_stop_is_not_terminal() {
    let unterminated = STOP.trim_end_matches('\n');
    assert!(!scan(&[START, DELTA, unterminated]).is_terminal());
}

#[test]
fn an_error_frame_is_never_terminal() {
    let error =
        "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n";
    assert!(!scan(&[START, error, STOP]).is_terminal());
    assert!(!scan(&[START, STOP, error]).is_terminal());
}

/// Frame-level, not substring-level.
#[test]
fn a_message_stop_mentioned_in_content_is_not_the_marker() {
    let mention =
        "event: content_block_delta\ndata: {\"delta\":{\"text\":\"event: message_stop\"}}\n\n";
    assert!(!scan(&[START, mention]).is_terminal());
}

/// What a transport break after `message_stop` keeps: every whole frame, and
/// not the partial one that was arriving when it broke — under either line
/// ending, and without inventing a terminator from a CR held at the end.
#[test]
fn a_partial_frame_after_the_marker_is_cut_from_the_kept_bytes() {
    let whole = format!("{START}{DELTA}{STOP}");
    let partial = "event: ping\ndata: {\"ty";
    assert_eq!(
        complete_frames_len(format!("{whole}{partial}").as_bytes()),
        whole.len()
    );
    assert_eq!(complete_frames_len(whole.as_bytes()), whole.len());
    let crlf = whole.replace('\n', "\r\n");
    assert_eq!(
        complete_frames_len(format!("{crlf}event: ping\r\n\r").as_bytes()),
        crlf.len()
    );
    assert_eq!(complete_frames_len(b"event: ping\n"), 0);
}

#[test]
fn a_whole_json_message_is_a_single_message() {
    assert!(is_single_message(
        br#"{"id":"msg_1","type":"message","role":"assistant","content":[]}"#
    ));
}

#[test]
fn a_truncated_json_message_is_not() {
    assert!(!is_single_message(
        br#"{"id":"msg_1","type":"message","con"#
    ));
    assert!(!is_single_message(b""));
}

#[test]
fn a_json_error_body_is_not_a_message() {
    assert!(!is_single_message(
        br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#
    ));
    assert!(!is_single_message(br#"[{"type":"message"}]"#));
}
