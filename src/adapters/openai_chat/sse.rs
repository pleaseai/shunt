//! Bounded incremental SSE decoding for OpenAI Chat response streams.

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
    /// The returned byte count lets the caller retain an unconsumed Bytes
    /// slice without copying it. Parser memory and translated output stay
    /// bounded by one event, independent of HTTP packetization.
    pub(super) fn push_one(&mut self, chunk: &[u8]) -> Result<(usize, Option<Item>), String> {
        if self.disposed {
            return Err("OpenAI Chat SSE parser is disposed".to_string());
        }
        for (offset, &byte) in chunk.iter().enumerate() {
            self.buffer.push(byte);
            if let Some(delimiter_len) = terminal_delimiter_len(&self.buffer) {
                let frame_len = self.buffer.len() - delimiter_len;
                if frame_len > self.max_event_bytes {
                    return Err(self.fail(format!(
                        "OpenAI Chat SSE event exceeded {} bytes",
                        self.max_event_bytes
                    )));
                }
                let mut frame = std::mem::take(&mut self.buffer);
                frame.truncate(frame_len);
                if self.done {
                    return Err(self.fail("OpenAI Chat SSE frame arrived after [DONE]"));
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
                    "OpenAI Chat SSE event exceeded {} bytes",
                    self.max_event_bytes
                )));
            }
        }
        Ok((chunk.len(), None))
    }

    pub(super) fn finish(&mut self) -> Result<(), String> {
        if self.disposed {
            return Err("OpenAI Chat SSE parser is disposed".to_string());
        }
        self.disposed = true;
        if self.buffer.iter().all(u8::is_ascii_whitespace) {
            self.buffer.clear();
            return Ok(());
        }
        Err(self.fail("OpenAI Chat SSE ended with an unterminated event frame"))
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
    let frame = std::str::from_utf8(frame)
        .map_err(|_| "invalid UTF-8 in OpenAI Chat SSE event".to_string())?;
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
        .map_err(|_| "invalid JSON in OpenAI Chat SSE event".to_string())
}
