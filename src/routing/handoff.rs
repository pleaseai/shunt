//! `[models.router.handoff_notes]` — telling the receiving tier why it got the
//! turn (ADR-0005 §6).
//!
//! The note is appended to the forwarded system prompt as a **new** text block
//! at the end, never merged into an existing one. The first block is Claude
//! Code's attribution block, which the upstream matches verbatim; rewriting it
//! would change what the client is identified as. Everything here is a pure
//! function of the decision plus the request tree, so the failover path is one
//! call and this file is where the rules are read.
//!
//! Cost: each toggle is a prompt-cache miss. Anthropic caching keys on the
//! serialized prefix, so the turn that adds the note and the turn that drops it
//! both write a fresh cache entry. That is why the note fires only on the turns
//! a signal actually decided, and why `only_on_wrong_signal_escalation`
//! defaults to `true`.

use serde_json::{json, Value};
use switchyard_libsy::DecisionSource;

use crate::config::HandoffNotesConfig;
use crate::routing::stage::{StageSource, StageTier};

/// The note this turn carries, or `None`.
///
/// `handed_off` is the first gate, and the caller derives it from the pin write
/// this turn committed — not from the decision, which is made against a pin read
/// before the write and can be overtaken. The note tells the receiving tier why
/// it got the turn, so a turn that moved nothing must not carry one. It is false
/// on the first turn of a session (no previous model to explain), on a
/// sessionless turn, on a turn whose signals merely *confirmed* the tier already
/// pinned — the case neither `tier` nor `source` can express, since a confirming
/// signal keeps its `Scorer(Override | Dimensions)` source — and on a turn whose
/// write lost the race to a concurrent turn that had already made the move.
///
/// A `Sticky` or `NoSignal` turn never carries one either: the first is a pin
/// holding through an estimate that did not earn a move, the second is the
/// picker's default. Neither is a handoff — nothing changed hands — and telling
/// the capable model "the previous model was stalling" on such a turn is the
/// exact false statement `only_on_wrong_signal_escalation` exists to prevent.
pub(crate) fn note_for(
    notes: &HandoffNotesConfig,
    tier: StageTier,
    source: StageSource,
    handed_off: bool,
) -> Option<&str> {
    if !handed_off {
        return None;
    }
    let scored = matches!(source, StageSource::Scorer(_));
    let signal_driven = matches!(
        source,
        StageSource::Scorer(DecisionSource::Override | DecisionSource::Dimensions)
    );
    match tier {
        StageTier::Capable => {
            let qualifies = signal_driven || (!notes.only_on_wrong_signal_escalation && scored);
            qualifies.then_some(notes.escalation_note.as_str())
        }
        StageTier::Efficient => notes.deescalation_note.as_deref().filter(|_| scored),
    }
}

/// Append `note` to the request's `system` as a new text block.
///
/// Returns whether the tree changed, which is
/// [`crate::request::RequestBody::mutate`]'s contract: it re-serializes the
/// passthrough bytes only on `true`, and a closure that lies about it leaves
/// the bytes and the tree disagreeing.
///
/// Three shapes, because Anthropic accepts all three:
///
/// * a string `system` becomes `[{type:text,text:<old>}, <note>]`, so the
///   original text survives as its own block rather than being concatenated;
/// * an array gets the note pushed on the end, leaving every existing block —
///   the attribution block first among them — byte-identical;
/// * an absent `system` is created as a one-block array.
///
/// Anything else (a number, an object) is left alone: the upstream will reject
/// it, and rewriting a malformed field into a well-formed one would turn a
/// client bug into a gateway-shaped success.
pub(crate) fn append_system_block(request: &mut Value, note: &str) -> bool {
    let block = json!({"type": "text", "text": note});
    let Some(object) = request.as_object_mut() else {
        return false;
    };
    match object.get_mut("system") {
        None => {
            object.insert("system".to_string(), Value::Array(vec![block]));
            true
        }
        Some(Value::Array(blocks)) => {
            blocks.push(block);
            true
        }
        Some(system @ Value::String(_)) => {
            let existing = std::mem::replace(system, Value::Null);
            *system = Value::Array(vec![json!({"type": "text", "text": existing}), block]);
            true
        }
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notes(gated: bool, deescalation: Option<&str>) -> HandoffNotesConfig {
        HandoffNotesConfig {
            escalation_note: "pick up the diagnosis".to_string(),
            deescalation_note: deescalation.map(str::to_string),
            only_on_wrong_signal_escalation: gated,
        }
    }

    #[test]
    fn a_gated_escalation_note_needs_a_signal_driven_decision() {
        let notes = notes(true, None);
        for source in [DecisionSource::Override, DecisionSource::Dimensions] {
            assert_eq!(
                note_for(
                    &notes,
                    StageTier::Capable,
                    StageSource::Scorer(source),
                    true
                ),
                Some("pick up the diagnosis"),
                "{source:?} is a signal-driven escalation"
            );
        }
        for source in [
            StageSource::Scorer(DecisionSource::FallOpen),
            StageSource::Scorer(DecisionSource::Ambiguous),
            StageSource::Sticky,
            StageSource::NoSignal,
        ] {
            assert_eq!(
                note_for(&notes, StageTier::Capable, source, true),
                None,
                "{source:?} must not claim the previous model was stalling"
            );
        }
    }

    /// Ungating widens the note to any *scored* turn — never to a pin that
    /// simply held, which is not a handoff at all.
    #[test]
    fn an_ungated_escalation_note_covers_every_scored_turn() {
        let notes = notes(false, None);
        assert_eq!(
            note_for(
                &notes,
                StageTier::Capable,
                StageSource::Scorer(DecisionSource::FallOpen),
                true
            ),
            Some("pick up the diagnosis")
        );
        for source in [StageSource::Sticky, StageSource::NoSignal] {
            assert_eq!(note_for(&notes, StageTier::Capable, source, true), None);
        }
    }

    #[test]
    fn the_deescalation_note_is_optional_and_scored() {
        let without = notes(true, None);
        assert_eq!(
            note_for(
                &without,
                StageTier::Efficient,
                StageSource::Scorer(DecisionSource::Dimensions),
                true
            ),
            None,
            "no note configured for the down direction"
        );

        let with = notes(true, Some("the plan is settled"));
        assert_eq!(
            note_for(
                &with,
                StageTier::Efficient,
                StageSource::Scorer(DecisionSource::Dimensions),
                true
            ),
            Some("the plan is settled")
        );
        assert_eq!(
            note_for(&with, StageTier::Efficient, StageSource::Sticky, true),
            None,
            "a pin that held is not a handoff"
        );
    }

    /// The gate the tier and the source cannot express: a signal that merely
    /// re-confirms the tier already pinned carries the *same*
    /// `Scorer(Dimensions)` as the signal that first earned it, so without
    /// `handed_off` every confirming turn re-appends the escalation note to a
    /// conversation that never changed hands.
    #[test]
    fn a_turn_that_handed_nothing_over_carries_no_note() {
        let escalation = notes(true, Some("the plan is settled"));
        for (tier, expected) in [
            (StageTier::Capable, "pick up the diagnosis"),
            (StageTier::Efficient, "the plan is settled"),
        ] {
            let source = StageSource::Scorer(DecisionSource::Dimensions);
            // Positive twin: the identical tier and source *do* produce the note
            // once the turn actually moved the tier, so the `None` below is the
            // handoff gate and not the source gate answering.
            assert_eq!(
                note_for(&escalation, tier, source, true),
                Some(expected),
                "{tier:?} on a real handoff"
            );
            assert_eq!(
                note_for(&escalation, tier, source, false),
                None,
                "{tier:?}: a confirming signal on an already-pinned tier hands nothing over"
            );
        }
    }

    /// The attribution block is the first element and the upstream matches it
    /// verbatim. Appending must leave it byte-identical.
    #[test]
    fn an_existing_first_block_is_untouched() {
        let attribution = json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."});
        let mut request = json!({"system": [attribution.clone()]});

        assert!(append_system_block(&mut request, "note"));

        assert_eq!(request["system"][0], attribution);
        assert_eq!(
            request["system"][1],
            json!({"type": "text", "text": "note"})
        );
        assert_eq!(request["system"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_string_system_becomes_two_blocks() {
        let mut request = json!({"system": "be direct"});

        assert!(append_system_block(&mut request, "note"));

        assert_eq!(
            request["system"],
            json!([
                {"type": "text", "text": "be direct"},
                {"type": "text", "text": "note"}
            ])
        );
    }

    #[test]
    fn an_absent_system_is_created() {
        let mut request = json!({"model": "claude-auto"});

        assert!(append_system_block(&mut request, "note"));

        assert_eq!(request["system"], json!([{"type": "text", "text": "note"}]));
    }

    /// A malformed `system` is the client's bug; rewriting it would hide it.
    #[test]
    fn a_malformed_system_is_left_alone() {
        let mut request = json!({"system": 7});

        assert!(!append_system_block(&mut request, "note"));

        assert_eq!(request["system"], json!(7));
    }

    /// The Responses translation reads an array `system` by joining every text
    /// block, so a two-block prompt reaches an OpenAI upstream intact. Pinned
    /// here because the note is what makes two blocks common.
    #[test]
    fn the_responses_path_reads_both_system_blocks() {
        let mut request = json!({
            "model": "claude-auto",
            "max_tokens": 16,
            "system": "be direct",
            "messages": [{"role": "user", "content": "hi"}],
        });
        assert!(append_system_block(&mut request, "pick up the diagnosis"));

        let route = crate::routing::Route {
            provider: "codex".to_string(),
            adapter: crate::routing::AdapterKind::Responses,
            model: "claude-auto".to_string(),
            upstream_model: "gpt-5.6-terra".to_string(),
            effort: None,
            service_tier: None,
        };
        let translated = crate::model::responses_request::translate_request_value(
            &request,
            &route,
            crate::config::ResponsesFlavor::OpenAi,
            false,
            None,
        );
        let instructions = translated["instructions"]
            .as_str()
            .expect("the translation carries instructions");

        assert!(
            instructions.contains("be direct") && instructions.contains("pick up the diagnosis"),
            "both blocks must survive translation, got: {instructions}"
        );
    }
}
