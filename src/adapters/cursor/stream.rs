use crate::adapters::cursor::{
    client::decode_frame_payload,
    connect::{parse_connect_error, ConnectFrameDecoder, FLAG_END},
    response::{CursorDecodeError, CursorStreamEvent},
    sse::{format_sse_error, CursorSseFramer},
};

pub struct CursorStreamMachine {
    decoder: ConnectFrameDecoder,
    framer: CursorSseFramer,
    finished: bool,
}

impl CursorStreamMachine {
    pub fn new(message_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            decoder: ConnectFrameDecoder::new(),
            framer: CursorSseFramer::new(message_id, model),
            finished: false,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        let frames = match self.decoder.push(chunk) {
            Ok(frames) => frames,
            Err(error) => {
                self.finished = true;
                return format_sse_error(&format!("Cursor frame decode failed: {error}"));
            }
        };
        for frame in frames {
            if frame.flags & FLAG_END != 0 {
                if let Some(error) = parse_connect_error(&frame.payload) {
                    self.finished = true;
                    let error = CursorDecodeError::ConnectEnd(error);
                    let mut output = self.framer.take_output();
                    output.extend_from_slice(&format_sse_error(&error.to_string()));
                    return output;
                }
                self.apply(CursorStreamEvent::End);
                continue;
            }
            if let Ok(message) = decode_frame_payload(&frame) {
                for event in crate::adapters::cursor::response::events_from_message(&message) {
                    self.apply(event);
                }
            }
        }
        self.framer.take_output()
    }

    pub fn finish(&mut self) -> Vec<u8> {
        if !self.finished {
            self.framer.finalize();
            self.finished = true;
        }
        self.framer.take_output()
    }

    fn apply(&mut self, event: CursorStreamEvent) {
        match event {
            CursorStreamEvent::ThinkingDelta { text } => self.framer.emit_thinking_delta(&text),
            CursorStreamEvent::TextDelta { text } => self.framer.emit_text_delta(&text),
            CursorStreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            } => self.framer.record_usage(
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            ),
            CursorStreamEvent::End => self.framer.emit_final_message("end_turn"),
            CursorStreamEvent::Session { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::cursor::{connect::encode_connect_frame, test_frames};

    #[test]
    fn emits_complete_frames_incrementally_across_chunks() {
        let frame = test_frames::text_frame("hello");
        let split = frame.len() / 2;
        let mut machine = CursorStreamMachine::new("msg_test", "cursor");
        assert!(machine.push(&frame[..split]).is_empty());
        let output = machine.push(&frame[split..]);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("message_start"));
        assert!(text.contains("hello"));
        let final_output = String::from_utf8(machine.finish()).unwrap();
        assert!(final_output.contains("message_stop"));
    }

    #[test]
    fn end_error_preserves_pending_output_then_emits_error() {
        let mut chunk = test_frames::text_frame("before error");
        let payload = serde_json::to_vec(&serde_json::json!({
            "error": {"code": "resource_exhausted", "message": "quota exceeded"}
        }))
        .unwrap();
        chunk.extend_from_slice(&encode_connect_frame(&payload, FLAG_END));
        let mut machine = CursorStreamMachine::new("msg_test", "cursor");
        let output = String::from_utf8(machine.push(&chunk)).unwrap();
        assert!(output.contains("before error"));
        assert!(output.contains("event: error"));
        assert!(output.contains("quota exceeded"));
    }
}
