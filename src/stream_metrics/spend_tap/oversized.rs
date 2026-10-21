//! Usage recovery for a Responses terminal frame the observer skipped as
//! oversized.
//!
//! A `response.completed` frame carries the whole `response.output` ahead of
//! `response.usage`, so a long answer can push it past the observer's parse
//! cap, after which its bytes are discarded unseen. OpenAI serializes `usage`
//! after `output`, so the end of the frame still holds it. [`OversizedTail`]
//! keeps a bounded window of that end and pulls the `usage` object out of it;
//! no relayed byte is held back or altered.

use super::responses::UsageFields;

/// Bytes of an oversized frame's end kept to look for `usage` in. A usage
/// block is a few hundred bytes, but the response's `user` and `metadata`
/// (up to 16 pairs of 64-character keys and 512-character values, ~9 KiB)
/// are serialized after it.
const WINDOW: usize = 16 * 1024;

/// How much of a frame's start is read to tell whether it is a terminal one.
const HEAD: usize = 1024;

const TERMINAL_MARKERS: [&[u8]; 4] = [
    b"response.completed",
    b"response.done",
    b"response.incomplete",
    b"response.failed",
];

pub(in crate::stream_metrics) struct OversizedTail {
    terminal: bool,
    window: Vec<u8>,
}

impl OversizedTail {
    /// Starts a window over a frame whose first `prefix` bytes were buffered.
    pub(super) fn begin(prefix: &[u8]) -> Self {
        let head = &prefix[..prefix.len().min(HEAD)];
        let mut tail = Self {
            terminal: TERMINAL_MARKERS
                .iter()
                .any(|marker| head.windows(marker.len()).any(|w| w == *marker)),
            window: Vec::new(),
        };
        tail.push(prefix);
        tail
    }

    /// Appends skipped bytes, keeping only the last [`WINDOW`].
    pub(in crate::stream_metrics) fn push(&mut self, bytes: &[u8]) {
        let bytes = &bytes[bytes.len().saturating_sub(WINDOW)..];
        self.window.extend_from_slice(bytes);
        if self.window.len() > 2 * WINDOW {
            let excess = self.window.len() - WINDOW;
            self.window.drain(..excess);
        }
    }

    /// The frame's `usage` object, when it is a terminal frame and the object
    /// lies whole inside the window.
    pub(super) fn usage(&self) -> Option<UsageFields> {
        if !self.terminal {
            return None;
        }
        let key: &[u8] = b"\"usage\"";
        let at = self.window.windows(key.len()).rposition(|w| w == key)?;
        let rest = trim_space(&self.window[at + key.len()..]);
        let rest = trim_space(rest.strip_prefix(b":")?);
        serde_json::from_slice(balanced_object(rest)?).ok()
    }
}

fn trim_space(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[start..]
}

/// The `{...}` object `bytes` starts with, when it closes inside `bytes`.
fn balanced_object(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.first() != Some(&b'{') {
        return None;
    }
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (index, &byte) in bytes.iter().enumerate() {
        if in_string {
            match (escaped, byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&bytes[..=index]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail(frame: &str) -> OversizedTail {
        let mut tail = OversizedTail::begin(&frame.as_bytes()[..64]);
        tail.push(&frame.as_bytes()[64..]);
        tail
    }

    #[test]
    fn reads_the_usage_object_after_a_long_output() {
        let frame = format!(
            r#"data: {{"type":"response.completed","response":{{"output":["{}"],"usage":{{"input_tokens":9,"output_tokens":3}}}}}}"#,
            "y".repeat(100_000)
        );
        let usage = tail(&frame).usage().expect("usage in the tail");
        assert!(usage.reports_input());
    }

    #[test]
    fn a_quoted_usage_key_inside_a_string_is_not_the_object() {
        let frame = format!(
            r#"data: {{"type":"response.completed","response":{{"output":["{}\"usage\":{{}}"]}}}}"#,
            "y".repeat(100_000)
        );
        assert!(tail(&frame).usage().is_none());
    }

    #[test]
    fn a_non_terminal_frame_or_a_cut_object_yields_nothing() {
        let delta = format!(
            r#"data: {{"type":"response.output_text.delta","delta":"{}","usage":{{"input_tokens":1}}}}"#,
            "y".repeat(100_000)
        );
        assert!(tail(&delta).usage().is_none());
        let cut = r#"data: {"type":"response.completed","response":{"output":[],"usage":{"input_tokens":1,"#;
        assert!(tail(&format!("{cut}{}", " ".repeat(100))).usage().is_none());
    }

    #[test]
    fn the_window_stays_bounded() {
        let mut tail = OversizedTail::begin(b"");
        for _ in 0..100 {
            tail.push(&[b'a'; 50_000]);
        }
        assert!(tail.window.len() <= 2 * WINDOW);
    }
}
