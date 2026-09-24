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

use crate::config::ToolSemanticsConfig;

use super::vocabulary::{classify, is_delegated_run, mutate_kind, MutateKind, ToolCategory};

/// One completed tool call: an assistant `tool_use` joined to the `tool_result`
/// that answered it.
struct Completed<'a> {
    /// The id the result named. Held unresolved so the walk can be a single
    /// pass: names are looked up afterwards, against the complete call map.
    id: &'a str,
    is_error: bool,
    recent: bool,
}

/// The tool name a completed call resolves to, or `""` when no `tool_use` in
/// this request claims its id — a call trimmed out of the history, or a body
/// that never sent one. Such a call is activity of unknown character, which is
/// what `Other` means.
///
/// Resolving here rather than during the walk is what keeps the join
/// independent of block order: `names` is already complete by the time any
/// call is classified, so a result that precedes its own call still joins.
fn name_of<'a>(names: &HashMap<&'a str, &'a str>, call: &Completed<'a>) -> &'a str {
    names.get(call.id).copied().unwrap_or("")
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
///
/// `semantics` is the operator's `[models.router.tool_semantics]` table, which
/// classifies names the built-in vocabulary leaves as `Other`. It is additive:
/// validation rejects any attempt to reclassify a built-in Observe/Mutate/Plan
/// name, so passing an empty table reproduces the built-in behaviour exactly.
pub(crate) fn extract(
    messages: &Value,
    recent_turn_window: usize,
    semantics: &ToolSemanticsConfig,
) -> Option<ToolSignals> {
    let messages = messages.as_array()?;

    // Pass 1 — walk backwards only as far as the window reaches, recording which
    // `tool_use` ids belong to the most recent assistant turns. Bounding the
    // scan here is what keeps a thousand-message transcript from being measured
    // to decide something that only ever looks at its tail.
    let mut recent: HashSet<&str> = HashSet::new();
    let mut assistant_turns = 0usize;
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        // Checked before the turn is read, so a window of 0 records nothing.
        if assistant_turns == recent_turn_window {
            break;
        }
        assistant_turns += 1;
        for block in content_blocks(message) {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            // Both fields, not just the id: a call this extractor cannot name
            // is one it cannot classify, so letting it into the window would
            // let a failed result read as a recent hard error on the strength
            // of a block nothing could read.
            let (Some(id), Some(_)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) else {
                continue;
            };
            recent.insert(id);
        }
    }

    // Pass 2 — walk forwards once, mapping each `tool_use` id to its tool name
    // and recording every result by the id it answered.
    //
    // The join is load-bearing: a `tool_result` block carries only
    // `tool_use_id`, never the tool's name, so without this map no result can
    // be classified at all. It stays a single pass because the names are
    // resolved after the walk rather than during it (see [`name_of`]) — the
    // wire format puts a call in an earlier message than the result answering
    // it, but `messages` is the client's body and nothing here enforces that.
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut completed = Vec::new();
    // Every assistant message, not the windowed count pass 1 keeps: libsy's
    // `assistant_turn_count` is a whole-request figure.
    let mut assistant_turn_count = 0u32;
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                assistant_turn_count += 1;
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
                }
            }
            Some("user") => {
                for block in content_blocks(message) {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    completed.push(Completed {
                        id,
                        // `is_error` lives on the result, never on the call.
                        is_error: block.get("is_error").and_then(Value::as_bool) == Some(true),
                        recent: recent.contains(id),
                    });
                }
            }
            _ => {}
        }
    }
    if completed.is_empty() {
        return None;
    }

    let mut signals = ToolSignals {
        turn_depth: messages.len() as u32,
        // Counted per block and including empty-content results, which is what
        // libsy counts: it tallies these before its own empty-text filter. Every
        // Anthropic `tool_result` carries a `tool_use_id`, so `completed` — which
        // is keyed on that field being present — is the same set.
        tool_result_count: completed.len() as u32,
        assistant_turn_count,
        // The rest are left at their defaults in this revision, deliberately:
        //
        // `tests_passed` is libsy's "a result after the latest recent failure
        // matched a test-pass pattern". Claude Code runs tests through `Bash`, so
        // deciding it means reading result *text*, and a structural proxy ("a
        // clean Bash result after a write") would fire for `ls` just as readily.
        //
        // `repeated_failure` is "the same hard-or-critical failure appeared at
        // least twice in the recent window" — libsy decides *same* by matching
        // result text against its error table. Claude Code reports only a boolean
        // `is_error` with no category, so there is nothing here to compare. It is
        // a hard escalation, so a proxy that guessed would escalate on any two
        // unrelated failures. The trailing-pair rule below already covers that
        // case through `severity`, on evidence this extractor actually has.
        //
        // `compacted` is not decided here. Its marker is a header, not the
        // transcript — `x-claude-code-context-compacted`, latched per session
        // by the store — so `stage::decide` writes it over this default from
        // the `RouterContext` after extraction (ADR-0005 §11).
        //
        // `new_count`/`recent_new_count` are filled below from the operator's
        // `[models.router.tool_semantics].new` list. They start at zero and stay
        // there when no such list is configured, which is the common case.
        tests_passed: false,
        repeated_failure: false,
        compacted: false,
        new_count: 0,
        recent_new_count: 0,
        ..ToolSignals::default()
    };

    for call in &completed {
        let name = name_of(&names, call);
        let category = classify(name, semantics);
        match category {
            ToolCategory::Observe => {
                signals.read_count += 1;
                signals.recent_read_count += u32::from(call.recent);
            }
            ToolCategory::Mutate => match mutate_kind(name) {
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
            ToolCategory::New => {
                signals.new_count += 1;
                signals.recent_new_count += u32::from(call.recent);
            }
            ToolCategory::Other => {}
        }
        // Severity is windowed so an error persists through the recovery turns
        // instead of clearing the moment one clean result lands.
        if call.is_error && call.recent {
            signals.severity = signals.severity.max(severity_of(name));
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
        .take_while(|call| classify(name_of(&names, call), semantics) == ToolCategory::Other)
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
/// `a_failed_task_outranks_a_failed_read`,
/// `severity_ignores_errors_outside_the_window`,
/// `both_plan_mode_tools_are_planning`, `a_skill_call_is_uncategorised`, and
/// `a_result_before_its_call_still_joins` all go red. Replace it with one
/// that returns `None` and every test but the three `None` cases goes red.
///
/// `a_nameless_call_is_not_recent` is the exception: it asserts an absence, so
/// a stub satisfies it. What makes it non-vacuous is the narrower mutation —
/// admit a `tool_use` to the recent set on its `id` alone, without requiring a
/// readable `name`, and it goes red at `severity 0.7`.
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
        extract(
            &Value::Array(messages),
            window,
            &ToolSemanticsConfig::default(),
        )
    }

    fn extract_with(messages: Vec<Value>, semantics: &ToolSemanticsConfig) -> Option<ToolSignals> {
        extract(&Value::Array(messages), 3, semantics)
    }

    /// An operator-declared `observe` name is counted as a read, so the scorer
    /// sees investigation where it previously saw activity of unknown
    /// character.
    #[test]
    fn an_operator_observe_name_counts_as_a_read() {
        let semantics = ToolSemanticsConfig {
            observe: vec!["mcp__jbcontext__code_search".to_string()],
            ..ToolSemanticsConfig::default()
        };
        let messages = vec![
            call("t1", "mcp__jbcontext__code_search"),
            result("t1", false),
        ];

        let default = extract_from(messages.clone(), 3).expect("signals");
        assert_eq!(default.read_count, 0, "an unlisted name is still unknown");
        assert_eq!(default.pure_bash_streak, 1);

        let listed = extract_with(messages, &semantics).expect("signals");
        assert_eq!(listed.read_count, 1);
        assert_eq!(listed.recent_read_count, 1);
        assert_eq!(
            listed.pure_bash_streak, 0,
            "a classified call is no longer activity of unknown character"
        );
    }

    /// An operator-declared `new` name reaches libsy's `new_count` pair, the
    /// two `ToolSignals` fields nothing else in shunt fills.
    #[test]
    fn an_operator_new_name_counts_into_new_count() {
        let semantics = ToolSemanticsConfig {
            new: vec!["Bash".to_string()],
            ..ToolSemanticsConfig::default()
        };

        let signals = extract_with(vec![call("t1", "Bash"), result("t1", false)], &semantics)
            .expect("signals");

        assert_eq!(signals.new_count, 1);
        assert_eq!(signals.recent_new_count, 1);
        assert_eq!(
            signals.read_count + signals.write_count + signals.todowrite_count,
            0,
            "`new` favours neither tier"
        );
    }

    /// A `mutate` name the operator declared has no sub-kind to read, so it is
    /// counted as a whole-file write rather than an in-place edit.
    #[test]
    fn an_operator_mutate_name_counts_as_a_write() {
        let semantics = ToolSemanticsConfig {
            mutate: vec!["mcp__fs__apply".to_string()],
            ..ToolSemanticsConfig::default()
        };

        let signals = extract_with(
            vec![call("t1", "mcp__fs__apply"), result("t1", false)],
            &semantics,
        )
        .expect("signals");

        assert_eq!(signals.write_count, 1);
        assert_eq!(signals.edit_count, 0);
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

    /// Both ways into plan mode are planning. Only the exit was named at first,
    /// which left an entered-but-not-exited plan turn scoring as nothing.
    #[test]
    fn both_plan_mode_tools_are_planning() {
        for name in ["EnterPlanMode", "ExitPlanMode"] {
            let signals =
                extract_from(vec![call("t1", name), result("t1", false)], 3).expect("signals");
            assert_eq!(signals.todowrite_count, 1, "{name} must classify as plan");
            assert_eq!(
                signals.pure_bash_streak, 0,
                "{name} is not unknown activity"
            );
        }
    }

    /// `Skill` names a dispatcher, not an activity — the skill it runs may plan
    /// or may rewrite the tree, and the call records only the name.
    #[test]
    fn a_skill_call_is_uncategorised() {
        let signals =
            extract_from(vec![call("t1", "Skill"), result("t1", false)], 3).expect("signals");

        assert_eq!(
            signals.todowrite_count, 0,
            "dispatching a skill is not evidence of planning"
        );
        assert_eq!(signals.pure_bash_streak, 1);
    }

    /// The wire format puts a call in an earlier message than the result
    /// answering it, but the body is the client's and nothing here enforces the
    /// order. The join must not depend on it — which is why names are resolved
    /// after the walk rather than during it.
    #[test]
    fn a_result_before_its_call_still_joins() {
        let signals = extract_from(vec![result("t1", false), call("t1", "Read")], 3)
            .expect("a result still counts as activity");

        assert_eq!(
            signals.read_count, 1,
            "the join must read the whole request, not only what preceded the result"
        );
        assert_eq!(signals.pure_bash_streak, 0);
    }

    /// A `tool_use` this extractor cannot name is one it cannot classify, so it
    /// must stay out of the recent window too. Letting it in would make a failed
    /// result a recent hard error on the strength of a block nothing could read.
    #[test]
    fn a_nameless_call_is_not_recent() {
        let signals = extract_from(
            vec![
                json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1"}]}),
                result("t1", true),
            ],
            3,
        )
        .expect("the result still counts as activity");

        assert_eq!(
            signals.severity, 0.0,
            "an unreadable call must not contribute a recent failure"
        );
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
