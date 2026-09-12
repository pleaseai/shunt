//! Anthropic Messages JSON → [`ToolSignals`].
//!
//! The scorer this feeds is `switchyard-libsy`'s, but its input type is a plain
//! struct of public counters, so shunt fills it straight from the buffered
//! request and never builds a `switchyard_protocol::Request`. That is what keeps
//! `switchyard-translation` — which would duplicate
//! `src/model/responses_request.rs` — out of the dependency tree.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use switchyard_libsy::ToolSignals;

use super::vocabulary::{classify, is_delegated_run, mutate_kind, MutateKind, ToolCategory};

/// One completed tool call: an assistant `tool_use` joined to the `tool_result`
/// that answered it.
struct Completed<'a> {
    name: &'a str,
    is_error: bool,
    recent: bool,
}

/// Severity of a single failed result, on libsy's scale (`0.0` clean, `0.3`
/// soft, `0.7` hard, `1.0` critical).
///
/// Claude Code reports only a boolean `is_error`, with no severity grading, so
/// every ordinary failure maps to `hard`. The one distinction the protocol does
/// support is *what* failed: a `Task` error is an entire delegated subagent run
/// collapsing, not one bad path, and is therefore critical.
fn severity_of(name: &str) -> f32 {
    if is_delegated_run(name) {
        1.0
    } else {
        0.7
    }
}

/// Extract tool-activity signals from a request's `messages` array.
///
/// Returns `None` when the conversation carries no completed tool call — there
/// is then no stage to estimate, and the caller must fall back to the picker
/// default rather than score an empty signal as "efficient".
///
/// `recent_turn_window` counts assistant messages, not tool calls: one turn is
/// an assistant message plus the user message answering it, so a turn that
/// batches eight results still counts once.
pub(crate) fn extract(messages: &Value, recent_turn_window: usize) -> Option<ToolSignals> {
    let messages = messages.as_array()?;

    // Pass 1 — map every `tool_use` id to its tool name, and record which ids
    // belong to the most recent assistant turns.
    //
    // The join is load-bearing: a `tool_result` block carries only
    // `tool_use_id`, never the tool's name, so without this map no result can
    // be classified at all.
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut turns: Vec<Vec<&str>> = Vec::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let mut turn = Vec::new();
        for block in content_blocks(message) {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) else {
                continue;
            };
            names.insert(id, name);
            turn.push(id);
        }
        turns.push(turn);
    }
    let recent: HashSet<&str> = turns
        .iter()
        .rev()
        .take(recent_turn_window)
        .flatten()
        .copied()
        .collect();

    // Pass 2 — walk the results in wire order, joining each back to its call.
    let mut completed = Vec::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        for block in content_blocks(message) {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            // A result whose call is not in this request (trimmed history, or a
            // malformed body) has no name and so no category. It still counts as
            // activity, which is what `Other` means.
            let name = names.get(id).copied().unwrap_or("");
            completed.push(Completed {
                name,
                // `is_error` lives on the result, never on the call.
                is_error: block.get("is_error").and_then(Value::as_bool) == Some(true),
                recent: recent.contains(id),
            });
        }
    }
    if completed.is_empty() {
        return None;
    }

    let mut signals = ToolSignals {
        turn_depth: messages.len() as u32,
        // Both left at their defaults in this revision, deliberately:
        //
        // `tests_passed` is libsy's "a recent result matched a test-pass
        // pattern". Claude Code runs tests through `Bash`, so deciding it means
        // reading result *text* — and it is a strong, immediate push to the
        // efficient tier, so guessing it wrong is expensive in exactly the
        // direction that hurts. A structural proxy ("a clean Bash result after a
        // write") would fire for `ls` just as readily.
        //
        // `compacted` needs a reliable marker for Claude Code's context-compaction
        // summary, which is not pinned by any test here yet.
        tests_passed: false,
        compacted: false,
        ..ToolSignals::default()
    };

    for call in &completed {
        let category = classify(call.name);
        match category {
            ToolCategory::Observe => {
                signals.read_count += 1;
                signals.recent_read_count += u32::from(call.recent);
            }
            ToolCategory::Mutate => match mutate_kind(call.name) {
                Some(MutateKind::Edit) => {
                    signals.edit_count += 1;
                    signals.recent_edit_count += u32::from(call.recent);
                }
                Some(MutateKind::Write) | None => {
                    signals.write_count += 1;
                    signals.recent_write_count += u32::from(call.recent);
                }
            },
            ToolCategory::Plan => {
                signals.todowrite_count += 1;
                signals.recent_todowrite_count += u32::from(call.recent);
            }
            ToolCategory::Other => {}
        }
        // Severity is windowed so an error persists through the recovery turns
        // instead of clearing the moment one clean result lands.
        if call.is_error && call.recent {
            signals.severity = signals.severity.max(severity_of(call.name));
        }
    }

    // Trailing streaks, newest first.
    signals.no_error_streak = completed
        .iter()
        .rev()
        .take_while(|call| !call.is_error)
        .count() as u32;
    signals.pure_bash_streak = completed
        .iter()
        .rev()
        .take_while(|call| classify(call.name) == ToolCategory::Other)
        .count() as u32;

    // Two consecutive failures at the end mean the session is not recovering,
    // which libsy's scale calls critical.
    if completed.len() >= 2
        && completed
            .iter()
            .rev()
            .take(2)
            .all(|call| call.is_error && call.recent)
    {
        signals.severity = 1.0;
    }

    Some(signals)
}

/// A message's content blocks. Anthropic allows `content` to be a bare string
/// instead of an array; such a message carries no tool blocks.
fn content_blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
}

/// Extraction tests.
///
/// Non-vacuity: replace [`extract`] with one that returns
/// `Some(ToolSignals::default())` and `joins_a_result_back_to_its_call`,
/// `counts_each_category`, `errors_are_read_from_the_result_not_the_call`,
/// `a_failed_task_outranks_a_failed_read`, and
/// `severity_ignores_errors_outside_the_window` all go red. Replace it with one
/// that returns `None` and every test but the three `None` cases goes red.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An assistant turn issuing one `tool_use`.
    fn call(id: &str, name: &str) -> Value {
        json!({"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": name}]})
    }

    /// The user turn answering it.
    fn result(id: &str, is_error: bool) -> Value {
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "is_error": is_error, "content": "out"}
        ]})
    }

    fn extract_from(messages: Vec<Value>, window: usize) -> Option<ToolSignals> {
        extract(&Value::Array(messages), window)
    }

    #[test]
    fn no_tool_activity_yields_no_signals() {
        // Each of these must be "no stage estimate", not a zeroed one: the
        // caller distinguishes them, falling back to the picker default.
        let cases: Vec<(&str, Vec<Value>)> = vec![
            ("empty", vec![]),
            (
                "plain text turns",
                vec![
                    json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
                    json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}]}),
                ],
            ),
            (
                // Anthropic allows a bare string here; it must not panic.
                "string content",
                vec![json!({"role": "user", "content": "hi"})],
            ),
            (
                // A call with no answer yet is not a completed call.
                "call without result",
                vec![call("t1", "Edit")],
            ),
        ];
        for (label, messages) in cases {
            assert!(
                extract_from(messages, 3).is_none(),
                "{label} must yield no signals"
            );
        }
    }

    /// A `tool_result` carries only `tool_use_id` — never the tool's name — so
    /// this join is the single point the whole extractor stands on.
    #[test]
    fn joins_a_result_back_to_its_call() {
        let signals = extract_from(vec![call("t1", "Edit"), result("t1", false)], 3)
            .expect("one completed call yields signals");

        assert_eq!(signals.edit_count, 1, "the Edit call must be counted");
        assert_eq!(signals.recent_edit_count, 1);
        assert_eq!(signals.read_count, 0);
    }

    #[test]
    fn an_unmatched_result_is_uncategorised_rather_than_a_panic() {
        let signals = extract_from(vec![result("orphan", false)], 3)
            .expect("a result still counts as completed activity");

        assert_eq!(
            signals.edit_count + signals.read_count + signals.todowrite_count,
            0
        );
        // Unknown activity is what `pure_bash_streak` measures.
        assert_eq!(signals.pure_bash_streak, 1);
    }

    #[test]
    fn counts_each_category() {
        let signals = extract_from(
            vec![
                call("t1", "Read"),
                result("t1", false),
                call("t2", "Write"),
                result("t2", false),
                call("t3", "TodoWrite"),
                result("t3", false),
                call("t4", "Bash"),
                result("t4", false),
            ],
            9,
        )
        .expect("signals");

        assert_eq!(signals.read_count, 1);
        assert_eq!(signals.write_count, 1);
        assert_eq!(signals.todowrite_count, 1);
        assert_eq!(signals.edit_count, 0, "Write is not Edit");
        // Bash stays deliberately unclassified.
        assert_eq!(signals.pure_bash_streak, 1);
    }

    #[test]
    fn tool_names_match_case_insensitively() {
        for name in ["Read", "read", "READ"] {
            let signals =
                extract_from(vec![call("t1", name), result("t1", false)], 3).expect("signals");
            assert_eq!(signals.read_count, 1, "{name} must classify as observe");
        }
    }

    #[test]
    fn an_mcp_tool_is_uncategorised() {
        let signals = extract_from(
            vec![call("t1", "mcp__github__search_code"), result("t1", false)],
            3,
        )
        .expect("signals");

        assert_eq!(signals.read_count, 0, "an unknown name must not be guessed");
        assert_eq!(signals.pure_bash_streak, 1);
    }

    /// `is_error` lives on the result. A body that puts it on the call instead
    /// must not register as a failure.
    #[test]
    fn errors_are_read_from_the_result_not_the_call() {
        let on_result = extract_from(vec![call("t1", "Read"), result("t1", true)], 3).unwrap();
        assert!(on_result.severity > 0.0, "a failed result raises severity");
        assert_eq!(on_result.no_error_streak, 0);

        let misplaced = extract_from(
            vec![
                json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "is_error": true}
                ]}),
                result("t1", false),
            ],
            3,
        )
        .unwrap();
        assert_eq!(
            misplaced.severity, 0.0,
            "is_error on the call must be ignored"
        );
        assert_eq!(misplaced.no_error_streak, 1);
    }

    /// A `Task` failure is a whole delegated subagent run collapsing, so it must
    /// outrank an ordinary tool failure rather than tie with it.
    #[test]
    fn a_failed_task_outranks_a_failed_read() {
        let task = extract_from(vec![call("t1", "Task"), result("t1", true)], 3).unwrap();
        let read = extract_from(vec![call("t1", "Read"), result("t1", true)], 3).unwrap();

        assert!(
            task.severity > read.severity,
            "Task {} must outrank Read {}",
            task.severity,
            read.severity
        );
    }

    /// The window is what stops a long transcript from carrying an ancient
    /// failure forever. With every error outside it, severity must be clean.
    #[test]
    fn severity_ignores_errors_outside_the_window() {
        let mut messages = Vec::new();
        for i in 0..10 {
            let id = format!("old{i}");
            messages.push(call(&id, "Read"));
            messages.push(result(&id, true));
        }
        for i in 0..3 {
            let id = format!("new{i}");
            messages.push(call(&id, "Edit"));
            messages.push(result(&id, false));
        }

        let signals = extract_from(messages, 3).expect("signals");

        assert_eq!(
            signals.severity, 0.0,
            "errors older than the window must not count"
        );
        assert_eq!(signals.recent_edit_count, 3);
        assert_eq!(
            signals.edit_count, 3,
            "unwindowed totals still see the whole request"
        );
        assert_eq!(signals.read_count, 10);
        assert_eq!(signals.recent_read_count, 0);
    }

    #[test]
    fn two_trailing_failures_are_critical() {
        let one = extract_from(
            vec![
                call("t1", "Edit"),
                result("t1", false),
                call("t2", "Read"),
                result("t2", true),
            ],
            3,
        )
        .unwrap();
        let two = extract_from(
            vec![
                call("t1", "Read"),
                result("t1", true),
                call("t2", "Read"),
                result("t2", true),
            ],
            3,
        )
        .unwrap();

        assert!(two.severity > one.severity, "not recovering is worse");
        assert_eq!(two.severity, 1.0);
    }
}
