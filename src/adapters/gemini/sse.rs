//! Bounded incremental SSE decoding for Gemini response streams.

use serde_json::Value;

pub(super) const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub(super) enum Item {
    Json(Value),
    Done,
}

pub(super) struct Decoder {
    buffer: Vec<u8>,
    max_event_bytes: usize,
    done: bool,
    disposed: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::with_limit(MAX_EVENT_BYTES)
    }
}

impl Decoder {
    pub(super) fn with_limit(max_event_bytes: usize) -> Self {
        assert!(max_event_bytes > 0);
        Self {
            buffer: Vec::new(),
            max_event_bytes,
            done: false,
            disposed: false,
        }
    }

    fn fail(&mut self, message: impl Into<String>) -> String {
        self.buffer.clear();
        self.disposed = true;
        message.into()
    }

    /// Consume no more than one complete SSE frame.
    ///
    /// The returned byte count lets the caller retain an unconsumed `Bytes`
    /// slice without copying it. Consequently parser memory and translated
    /// output are bounded by one event, independent of HTTP packetization.
    pub(super) fn push_one(&mut self, chunk: &[u8]) -> Result<(usize, Option<Item>), String> {
        if self.disposed {
            return Err("Gemini SSE parser is disposed".to_string());
        }
        for (offset, &byte) in chunk.iter().enumerate() {
            self.buffer.push(byte);
            if let Some(delimiter_len) = terminal_delimiter_len(&self.buffer) {
                let frame_len = self.buffer.len() - delimiter_len;
                if frame_len > self.max_event_bytes {
                    return Err(self.fail(format!(
                        "Gemini SSE event exceeded {} bytes",
                        self.max_event_bytes
                    )));
                }
                let mut frame = std::mem::take(&mut self.buffer);
                frame.truncate(frame_len);
                if self.done {
                    return Err(self.fail("Gemini SSE frame arrived after [DONE]"));
                }
                let item = match parse_frame(&frame) {
                    Ok(Some(Item::Done)) => {
                        self.done = true;
                        Some(Item::Done)
                    }
                    Ok(item) => item,
                    Err(error) => return Err(self.fail(error)),
                };
                return Ok((offset + 1, item));
            } else if retained_candidate_len(&self.buffer) > self.max_event_bytes {
                return Err(self.fail(format!(
                    "Gemini SSE event exceeded {} bytes",
                    self.max_event_bytes
                )));
            }
        }
        Ok((chunk.len(), None))
    }

    pub(super) fn finish(&mut self) -> Result<(), String> {
        if self.disposed {
            return Err("Gemini SSE parser is disposed".to_string());
        }
        self.disposed = true;
        if self.buffer.iter().all(u8::is_ascii_whitespace) {
            self.buffer.clear();
            return Ok(());
        }
        Err(self.fail("Gemini SSE ended with an unterminated event frame"))
    }
}

fn retained_candidate_len(buffer: &[u8]) -> usize {
    const PREFIXES: &[&[u8]] = &[b"\r", b"\n", b"\r\n", b"\n\r", b"\r\n\r", b"\n\r\n"];
    let delimiter_prefix = PREFIXES
        .iter()
        .filter(|prefix| buffer.ends_with(prefix))
        .map(|prefix| prefix.len())
        .max()
        .unwrap_or(0);
    buffer.len().saturating_sub(delimiter_prefix)
}

fn terminal_delimiter_len(buffer: &[u8]) -> Option<usize> {
    [b"\r\n\r\n".as_slice(), b"\n\r\n", b"\r\n\n", b"\n\n"]
        .into_iter()
        .find(|delimiter| buffer.ends_with(delimiter))
        .map(<[u8]>::len)
}

fn parse_frame(frame: &[u8]) -> Result<Option<Item>, String> {
    let frame =
        std::str::from_utf8(frame).map_err(|_| "invalid UTF-8 in Gemini SSE event".to_string())?;
    let mut data = Vec::new();
    for line in frame.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(':') {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        if field == "data" {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    let data = data.join("\n");
    if data.is_empty() {
        return Ok(None);
    }
    if data == "[DONE]" {
        return Ok(Some(Item::Done));
    }
    serde_json::from_str(&data)
        .map(Item::Json)
        .map(Some)
        .map_err(|_| "invalid JSON in Gemini SSE event".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn drain(decoder: &mut Decoder, wire: &[u8]) -> Result<Vec<Item>, String> {
        let mut offset = 0;
        let mut items = Vec::new();
        while offset < wire.len() {
            let (consumed, item) = decoder.push_one(&wire[offset..])?;
            assert!(consumed > 0);
            offset += consumed;
            if let Some(item) = item {
                items.push(item);
            }
        }
        Ok(items)
    }

    #[test]
    fn gemini_sse_bounds_preserves_splits_framing_and_event_order() {
        let wire = concat!(
            ": comment\r\n",
            "event: message\r\n",
            "unknown: ignored\r\n",
            "data: {\"text\":\"Olá ",
            "🌊\",\r\n",
            "data: \"n\":1}\r\n\r\n",
            "data:{\"n\":2}\n\r\n",
        );
        let wave = wire.find('🌊').unwrap();
        let mut decoder = Decoder::with_limit(128);

        assert_eq!(
            decoder.push_one(&wire.as_bytes()[..wave + 1]).unwrap(),
            (wave + 1, None)
        );
        let items = drain(&mut decoder, &wire.as_bytes()[wave + 1..]).unwrap();

        assert_eq!(
            items,
            vec![
                Item::Json(json!({"text": "Olá 🌊", "n": 1})),
                Item::Json(json!({"n": 2}))
            ]
        );
    }

    #[test]
    fn gemini_sse_bounds_accepts_exact_event_and_rejects_plus_one() {
        let frame = b"data: {}";
        let mut exact = Decoder::with_limit(frame.len());
        let mut exact_wire = frame.to_vec();
        exact_wire.extend_from_slice(b"\n\n");
        assert_eq!(
            exact.push_one(&exact_wire).unwrap(),
            (exact_wire.len(), Some(Item::Json(json!({}))))
        );

        let mut oversized = Decoder::with_limit(frame.len() - 1);
        assert!(oversized
            .push_one(&exact_wire)
            .unwrap_err()
            .contains("exceeded"));
        assert!(oversized
            .push_one(b"data: {}\n\n")
            .unwrap_err()
            .contains("disposed"));
    }

    #[test]
    fn gemini_sse_bounds_decodes_one_frame_per_step_without_feed_amplification() {
        let wire = b"data: {\"n\":1}\n\ndata: {\"n\":2}\n\ndata: {\"n\":3}\n\n";
        let mut decoder = Decoder::with_limit(64);
        assert_eq!(
            drain(&mut decoder, wire).unwrap(),
            vec![
                Item::Json(json!({"n": 1})),
                Item::Json(json!({"n": 2})),
                Item::Json(json!({"n": 3})),
            ]
        );
    }

    #[test]
    fn gemini_sse_bounds_rejects_invalid_utf8_and_json_then_disposes() {
        for bad in [b"data: \xff\n\n".as_slice(), b"data: nope\n\n".as_slice()] {
            let mut decoder = Decoder::with_limit(64);
            assert!(decoder.push_one(bad).is_err());
            assert!(decoder
                .push_one(b"data: {}\n\n")
                .unwrap_err()
                .contains("disposed"));
        }
    }

    #[test]
    fn gemini_sse_bounds_requires_clean_termination() {
        let mut clean = Decoder::with_limit(64);
        assert_eq!(clean.push_one(b" \r\n").unwrap(), (3, None));
        clean.finish().unwrap();

        let mut cut = Decoder::with_limit(64);
        let incomplete = b"data: {\"n\":1}";
        assert_eq!(cut.push_one(incomplete).unwrap(), (incomplete.len(), None));
        assert!(cut.finish().unwrap_err().contains("unterminated"));
        assert!(cut.push_one(b"\n\n").unwrap_err().contains("disposed"));
    }

    #[test]
    fn gemini_sse_bounds_treats_empty_data_as_noop_and_done_as_terminal_boundary() {
        let mut decoder = Decoder::with_limit(64);
        assert_eq!(
            drain(&mut decoder, b"data:\n\ndata: [DONE]\n\n").unwrap(),
            vec![Item::Done]
        );
        assert!(decoder
            .push_one(b"data: {\"late\":true}\n\n")
            .unwrap_err()
            .contains("after [DONE]"));
    }

    #[test]
    fn gemini_post_done_frames() {
        const DONE: &[u8] = b"data: [DONE]\n\n";
        let suffixes = [
            b": keepalive\n\n".as_slice(),
            b"data:\n\n",
            b"unknown: ignored\n\n",
            b"data: {}\n\n",
            b"data: [DONE]\n\n",
        ];

        for suffix in suffixes {
            let combined = [DONE, suffix].concat();
            let mut coalesced = Decoder::with_limit(64);
            let (used, item) = coalesced.push_one(&combined).unwrap();
            assert_eq!(used, DONE.len());
            assert_eq!(item, Some(Item::Done));
            assert!(coalesced.push_one(&combined[used..]).is_err());
            assert!(coalesced.push_one(b"data: {}\n\n").is_err());

            let mut aligned = Decoder::with_limit(64);
            assert_eq!(aligned.push_one(DONE).unwrap().1, Some(Item::Done));
            assert!(aligned.push_one(suffix).is_err());

            let mut delimiter_split = Decoder::with_limit(64);
            assert_eq!(delimiter_split.push_one(DONE).unwrap().1, Some(Item::Done));
            let cut = suffix.len() - 1;
            assert_eq!(
                delimiter_split.push_one(&suffix[..cut]).unwrap(),
                (cut, None)
            );
            assert!(delimiter_split.push_one(&suffix[cut..]).is_err());
        }

        let mut whitespace = Decoder::with_limit(64);
        assert_eq!(whitespace.push_one(DONE).unwrap().1, Some(Item::Done));
        assert_eq!(whitespace.push_one(b" \r\n").unwrap(), (3, None));
        whitespace.finish().unwrap();
    }

    #[test]
    fn gemini_sse_valid_prefix_survives_a_later_malformed_frame_regardless_of_split() {
        let valid = b"data: {\"n\":1}\n\n";
        let invalid = b"data: nope\n\n";
        let mut combined = Decoder::with_limit(64);
        let (used, item) = combined
            .push_one(&[valid.as_slice(), invalid.as_slice()].concat())
            .unwrap();
        assert_eq!(used, valid.len());
        assert_eq!(item, Some(Item::Json(json!({"n": 1}))));
        assert!(combined
            .push_one(invalid)
            .unwrap_err()
            .contains("invalid JSON"));

        let mut split = Decoder::with_limit(64);
        assert_eq!(
            split.push_one(valid).unwrap().1,
            Some(Item::Json(json!({"n": 1})))
        );
        assert!(split
            .push_one(invalid)
            .unwrap_err()
            .contains("invalid JSON"));
    }
}
