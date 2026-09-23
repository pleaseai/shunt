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
//! goes red; drop the `errored` check and
//! `an_error_frame_before_the_marker_is_never_terminal` goes red; keep reading
//! after `message_stop`, or record where the chunk ended rather than where the
//! frame did, and `nothing_after_message_stop_is_part_of_the_turn` goes red;
//! accept any JSON object in [`is_single_message`] and
//! `a_json_error_body_is_not_a_message` goes red; let any line ending, not
//! only one that closes a blank line, end a frame in [`first_frame_len`], or
//! stop holding back its trailing CR, and
//! `a_frame_ends_only_at_a_blank_line` goes red.

use super::{first_frame_len, is_single_message, TerminalScan};

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

const ERROR: &str =
    "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n";

#[test]
fn an_error_frame_before_the_marker_is_never_terminal() {
    assert!(!scan(&[START, ERROR, STOP]).is_terminal());
}

/// The live relay ends the client's stream at `message_stop`, so a frame after
/// it — a keep-alive, even an `error` — is not part of the turn, and the turn
/// is exactly the bytes through the marker, however the chunks fell.
#[test]
fn nothing_after_message_stop_is_part_of_the_turn() {
    let turn = format!("{START}{DELTA}{STOP}");
    let ping = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
    for tail in [ping, ERROR, "event: ping\ndata: {\"ty"] {
        let joined = scan(&[&format!("{turn}{tail}")]);
        assert!(joined.is_terminal(), "tail: {tail}");
        assert_eq!(joined.terminal_len(), Some(turn.len()), "tail: {tail}");
        let separate = scan(&[START, DELTA, STOP, tail]);
        assert!(separate.is_terminal(), "tail: {tail}");
        assert_eq!(separate.terminal_len(), Some(turn.len()), "tail: {tail}");
    }
    // A marker split across chunks ends where its frame did, not where the
    // chunk that completed it did.
    let (head, rest) = STOP.split_at(9);
    let split = scan(&[START, DELTA, head, &format!("{rest}{ping}")]);
    assert_eq!(split.terminal_len(), Some(turn.len()));
    // Measured on the raw bytes, so a CRLF turn is cut at its own length.
    let crlf = turn.replace('\n', "\r\n");
    let crlf_scan = scan(&[&crlf, &ping.replace('\n', "\r\n")]);
    assert_eq!(crlf_scan.terminal_len(), Some(crlf.len()));
    assert_eq!(scan(&[START, DELTA]).terminal_len(), None);
}

/// Frame-level, not substring-level.
#[test]
fn a_message_stop_mentioned_in_content_is_not_the_marker() {
    let mention =
        "event: content_block_delta\ndata: {\"delta\":{\"text\":\"event: message_stop\"}}\n\n";
    assert!(!scan(&[START, mention]).is_terminal());
}

/// A frame ends at its blank line — under any line ending, and without
/// inventing a terminator from a CR held at the end.
#[test]
fn a_frame_ends_only_at_a_blank_line() {
    assert_eq!(
        first_frame_len(format!("{START}{DELTA}").as_bytes()),
        Some(START.len())
    );
    let crlf = START.replace('\n', "\r\n");
    assert_eq!(
        first_frame_len(format!("{crlf}event: ping").as_bytes()),
        Some(crlf.len())
    );
    let bare_cr = START.replace('\n', "\r");
    assert_eq!(
        first_frame_len(format!("{bare_cr}event: ping").as_bytes()),
        Some(bare_cr.len())
    );
    assert_eq!(first_frame_len(b"event: ping\r\n\r"), None);
    assert_eq!(first_frame_len(b"event: ping\n"), None);
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
