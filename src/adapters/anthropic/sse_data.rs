//! Read and rewrite the `data` payload of one Anthropic SSE frame.
//!
//! The SSE spec lets one event spread its data over several `data:` lines,
//! which a reader joins with `\n` before dispatching. No Anthropic-kind
//! upstream is known to do that, but it is valid SSE, and a frame read one
//! line at a time would not parse — and so would pass through unrewritten
//! (issue #788). Both the model-alias rewrite and the thinking-signature wrap
//! read frames here so neither keeps that single-line assumption.

use serde_json::Value;

/// The JSON payload of a frame: every `data:` line's value, joined with `\n`.
///
/// Per the SSE spec, a line's trailing `\r` is not part of it, and `data:` is
/// followed by at most one ignored space. A frame with no `data:` line, or
/// whose joined data is not JSON, yields `None`.
pub(super) fn frame_data(frame: &str) -> Option<Value> {
    let mut values = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|value| value.strip_prefix(' ').unwrap_or(value));
    let first = values.next()?;
    match values.next() {
        // The common single-line frame is parsed in place, without a copy.
        None => serde_json::from_str(first).ok(),
        Some(second) => {
            let mut joined = format!("{first}\n{second}");
            for value in values {
                joined.push('\n');
                joined.push_str(value);
            }
            serde_json::from_str(&joined).ok()
        }
    }
}

/// The frame with its data replaced by `event`: the first `data:` line becomes
/// `data: <event>` and keeps its own line ending, every later `data:` line is
/// dropped with its ending, and every other line is kept byte for byte.
pub(super) fn replace_data(frame: &str, event: &Value) -> String {
    let mut out = String::with_capacity(frame.len());
    let mut replaced = false;
    for segment in frame.split_inclusive('\n') {
        let content_len = segment.trim_end_matches(['\n', '\r']).len();
        let (content, ending) = segment.split_at(content_len);
        if !content.starts_with("data:") {
            out.push_str(segment);
        } else if !replaced {
            out.push_str("data: ");
            out.push_str(&event.to_string());
            out.push_str(ending);
            replaced = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{frame_data, replace_data};

    #[test]
    fn data_lines_are_joined_with_a_newline() {
        // `data:` with and without the one optional space, CRLF line endings:
        // the joined value is `{"a":\n1}`, which is JSON only if joined.
        let frame = "event: x\r\ndata: {\"a\":\r\ndata:1}\r\n\r\n";
        assert_eq!(frame_data(frame), Some(json!({"a": 1})));
        // Only one space is stripped: a second is part of the value, which
        // JSON tolerates, so the parse still succeeds.
        assert_eq!(
            frame_data("data:  {\"b\":\ndata: 2}\n\n"),
            Some(json!({"b": 2}))
        );
        assert_eq!(frame_data("event: ping\n\n"), None, "no data line");
        assert_eq!(frame_data("data: {\"a\":\n\n"), None, "not JSON");
    }

    #[test]
    fn a_single_data_line_parses_as_before() {
        assert_eq!(
            frame_data("event: ping\ndata: {\"type\":\"ping\"}\n\n"),
            Some(json!({"type": "ping"}))
        );
    }

    #[test]
    fn replace_keeps_other_lines_and_drops_extra_data_lines() {
        let frame = "event: x\r\nid: 7\r\ndata: {\"a\":\r\ndata: 1}\r\n: comment\r\n\r\n";
        assert_eq!(
            replace_data(frame, &json!({"a": 2})),
            "event: x\r\nid: 7\r\ndata: {\"a\":2}\r\n: comment\r\n\r\n"
        );
        let single = "event: x\ndata: {\"a\":1}\n\n";
        assert_eq!(
            replace_data(single, &json!({"a": 1})),
            single,
            "a single-line frame keeps its shape"
        );
    }
}
