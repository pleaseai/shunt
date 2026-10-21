//! The OpenAI Responses side of [`StreamSpend`](super::StreamSpend): what one
//! Responses SSE frame — the inbound Codex endpoint's stream — contributes to a
//! turn's bill.
//!
//! - A terminal frame (`response.completed`, `response.done`,
//!   `response.incomplete` — the set [`super::super::observe_responses`] treats
//!   as a concluded stream) carries the turn's `response.usage`, and so may a
//!   `response.failed`. An `error` frame carries none.
//! - A turn whose usage never reported its input — cut before its terminal,
//!   or failed with `null` usage — bills an input estimate instead
//!   ([`RequestBody`](super::RequestBody)).
//! - A generated-text delta carries `delta` text, counted toward the
//!   delivered-output floor exactly like an Anthropic `content_block_delta`.
//!
//! Responses usage differs from Anthropic's in one way that costs money:
//! `input_tokens` *includes* `input_tokens_details.cached_tokens`, while the
//! pricing [`Usage`](crate::gateway::spend::pricing::Usage) bills uncached
//! input and cache reads separately. [`UsageFields::apply_to`] splits them,
//! saturating, so a backend that reports more cached than total input bills
//! zero uncached input instead of wrapping. Reasoning tokens are already inside
//! `output_tokens`.

use std::borrow::Cow;

use serde::Deserialize;

use crate::gateway::spend::meter::RequestUsage;

/// Terminal events whose `response.usage` is the turn's final count.
const TERMINAL_EVENTS: [&[u8]; 3] = [
    b"response.completed",
    b"response.done",
    b"response.incomplete",
];

/// A failed turn: not terminal for the stream metrics (they count it as an
/// error), but its `response.usage`, when the backend fills it, is what the
/// turn cost. `null` usage leaves the bill to the floor (the estimated input
/// plus the delivered output), as an `error` frame (which carries no usage)
/// does.
const FAILED_EVENT: &[u8] = b"response.failed";

/// Events whose `delta` is generated output the client was sent: answer and
/// refusal text, tool-call arguments, and reasoning text — the Responses twins
/// of the Anthropic `text`, `partial_json` and `thinking` deltas.
const DELTA_EVENTS: [&[u8]; 6] = [
    b"response.output_text.delta",
    b"response.refusal.delta",
    b"response.function_call_arguments.delta",
    b"response.custom_tool_call_input.delta",
    b"response.reasoning_summary_text.delta",
    b"response.reasoning_text.delta",
];

/// What one Responses frame tells the spend accounting.
#[derive(Debug, PartialEq)]
pub(super) enum Observed {
    /// Characters of delivered generated text.
    Delivered(u64),
    /// The terminal usage block.
    Usage(UsageFields),
    Nothing,
}

/// Reads one complete frame's event name and `data:` value. A frame with no
/// `event:` line is named by its payload's `type`, as the Codex CLI reads it.
/// Malformed JSON observes nothing: the frame was still relayed, and the floor
/// covers output whose usage never parsed.
pub(super) fn observe(event: Option<&[u8]>, data: &[u8]) -> Observed {
    let payload_type;
    let event = match event {
        Some(event) => event,
        None => match serde_json::from_slice::<PayloadType<'_>>(data) {
            Ok(PayloadType { kind }) => {
                payload_type = kind;
                payload_type.as_bytes()
            }
            Err(_) => return Observed::Nothing,
        },
    };
    if DELTA_EVENTS.contains(&event) {
        return serde_json::from_slice::<Delta<'_>>(data).map_or(Observed::Nothing, |frame| {
            Observed::Delivered(frame.delta.chars().count() as u64)
        });
    }
    if TERMINAL_EVENTS.contains(&event) || event == FAILED_EVENT {
        if let Ok(Terminal {
            response: ResponseHolder { usage: Some(usage) },
        }) = serde_json::from_slice(data)
        {
            return Observed::Usage(usage);
        }
    }
    Observed::Nothing
}

#[derive(Deserialize)]
struct PayloadType<'a> {
    #[serde(borrow, rename = "type")]
    kind: Cow<'a, str>,
}

#[derive(Deserialize)]
struct Delta<'a> {
    #[serde(borrow)]
    delta: Cow<'a, str>,
}

#[derive(Deserialize)]
struct Terminal {
    response: ResponseHolder,
}

/// The `response` object of a terminal frame — and, for a non-streamed turn,
/// the body itself.
#[derive(Deserialize)]
pub(super) struct ResponseHolder {
    pub(super) usage: Option<UsageFields>,
}

/// The billable fields of a Responses `usage` block. Unknown fields
/// (`total_tokens`, `output_tokens_details`, …) are ignored, and a field the
/// block omits or nulls is not reported.
#[derive(Debug, Default, Deserialize, PartialEq)]
pub(super) struct UsageFields {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    input_tokens_details: Option<InputDetails>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
struct InputDetails {
    cached_tokens: Option<u64>,
}

impl UsageFields {
    /// Whether this block counts the turn's input. Without one the floor
    /// estimates the input from the request body.
    pub(super) fn reports_input(&self) -> bool {
        self.input_tokens.is_some()
    }

    /// Overwrites `usage` with this block, splitting the cached tokens out of
    /// the input. Returns whether the block reported `output_tokens` — the
    /// upstream's own final count, which the floor does not override.
    pub(super) fn apply_to(&self, usage: &mut RequestUsage) -> bool {
        let tokens = &mut usage.tokens;
        let cached = self
            .input_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens);
        if let Some(input) = self.input_tokens {
            tokens.input_tokens = input.saturating_sub(cached.unwrap_or(0));
        }
        if let Some(cached) = cached {
            tokens.cache_read_input_tokens = cached;
        }
        if let Some(output) = self.output_tokens {
            tokens.output_tokens = output;
        }
        self.output_tokens.is_some()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::gateway::spend::pricing::Usage;

    fn completed(usage: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "usage": usage},
        }))
        .unwrap()
    }

    fn applied(event: &str, data: &[u8]) -> (Usage, bool) {
        let Observed::Usage(fields) = observe(Some(event.as_bytes()), data) else {
            panic!("{event} carried no usage");
        };
        let mut usage = RequestUsage::default();
        let reported = fields.apply_to(&mut usage);
        (usage.tokens, reported)
    }

    #[test]
    fn cached_tokens_are_split_out_of_the_input() {
        let data = completed(json!({
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 600},
            "output_tokens": 50,
            "output_tokens_details": {"reasoning_tokens": 20},
            "total_tokens": 1050,
        }));
        assert_eq!(
            applied("response.completed", &data),
            (
                Usage {
                    input_tokens: 400,
                    output_tokens: 50,
                    cache_read_input_tokens: 600,
                    cache_creation_input_tokens: 0,
                },
                true
            )
        );
    }

    #[test]
    fn more_cached_than_input_bills_zero_uncached_input() {
        let data = completed(json!({
            "input_tokens": 10,
            "input_tokens_details": {"cached_tokens": 25},
            "output_tokens": 3,
        }));
        let (usage, _) = applied("response.completed", &data);
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 25);
        assert_eq!(usage.output_tokens, 3);
    }

    #[test]
    fn every_terminal_event_carries_usage_and_null_details_are_uncached() {
        let data = completed(json!({
            "input_tokens": 7,
            "input_tokens_details": null,
            "output_tokens": null,
        }));
        for event in ["response.completed", "response.done", "response.incomplete"] {
            assert_eq!(
                applied(event, &data),
                (
                    Usage {
                        input_tokens: 7,
                        ..Usage::default()
                    },
                    false
                ),
                "{event}"
            );
        }
    }

    #[test]
    fn a_failed_turn_carries_its_usage_and_an_error_frame_carries_none() {
        let failed = serde_json::to_vec(&json!({
            "type": "response.failed",
            "response": {"id": "resp_1", "status": "failed",
                         "usage": {"input_tokens": 40, "output_tokens": 6}},
        }))
        .unwrap();
        let (usage, reported) = applied("response.failed", &failed);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, reported),
            (40, 6, true)
        );
        let Observed::Usage(fields) = observe(None, &failed) else {
            panic!("an untyped response.failed carried no usage");
        };
        assert!(fields.reports_input());

        let null_usage = br#"{"type":"response.failed","response":{"id":"resp_1","usage":null}}"#;
        assert_eq!(observe(None, null_usage), Observed::Nothing);
        let error = br#"{"type":"error","code":"server_error","message":"boom"}"#;
        assert_eq!(observe(Some(b"error"), error), Observed::Nothing);
    }

    #[test]
    fn a_block_without_input_tokens_does_not_report_input() {
        let data = completed(json!({"output_tokens": 4}));
        let Observed::Usage(fields) = observe(None, &data) else {
            panic!("no usage");
        };
        assert!(!fields.reports_input());
    }

    /// The Codex backend may send `data:`-only frames; their `type` names them.
    #[test]
    fn a_frame_without_an_event_line_is_named_by_its_type() {
        let data = completed(json!({"input_tokens": 9, "output_tokens": 2}));
        let Observed::Usage(fields) = observe(None, &data) else {
            panic!("an untyped terminal carried no usage");
        };
        let mut usage = RequestUsage::default();
        fields.apply_to(&mut usage);
        assert_eq!(
            usage.tokens,
            Usage {
                input_tokens: 9,
                output_tokens: 2,
                ..Usage::default()
            }
        );
        assert_eq!(
            observe(
                None,
                br#"{"type":"response.output_text.delta","delta":"abc"}"#
            ),
            Observed::Delivered(3)
        );
    }

    #[test]
    fn generated_deltas_count_characters_and_other_events_observe_nothing() {
        // Five characters, six bytes: the floor counts characters.
        let delta =
            serde_json::to_vec(&json!({"type": "response.output_text.delta", "delta": "héllo"}))
                .unwrap();
        for event in DELTA_EVENTS {
            assert_eq!(observe(Some(event), &delta), Observed::Delivered(5));
        }
        assert_eq!(
            observe(Some(b"response.refusal.delta"), &delta),
            Observed::Delivered(5)
        );
        let usage = completed(json!({"input_tokens": 1, "output_tokens": 1}));
        assert_eq!(
            observe(Some(b"response.created"), &usage),
            Observed::Nothing
        );
        assert_eq!(observe(None, br#"{"delta":"untyped"}"#), Observed::Nothing);
        assert_eq!(
            observe(Some(b"response.completed"), b"not json"),
            Observed::Nothing
        );
        assert_eq!(
            observe(Some(b"response.output_text.delta"), b"{\"delta\":7}"),
            Observed::Nothing
        );
    }
}
