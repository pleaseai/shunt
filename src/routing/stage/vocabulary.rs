//! Claude Code's tool names, mapped onto the stage router's activity categories.
//!
//! Switchyard ships a provider-neutral vocabulary because it cannot know which
//! agent is calling. shunt can: the inbound protocol is Claude Code's, and its
//! tool names are a stable part of that contract. Naming them directly is the
//! one place this router can be more accurate than the generic algorithm.

use crate::config::ToolSemanticsConfig;

/// What one tool call says about where the session is in its work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolCategory {
    /// Read-only lookup or inspection. Investigative, non-producing.
    Observe,
    /// Changes file or task state. Production.
    Mutate,
    /// Explicit planning or delegation. Investigative, non-producing.
    Plan,
    /// Forward activity an operator declared as such, counted into libsy's
    /// `new_count`/`recent_new_count`. Only reachable through
    /// `[models.router.tool_semantics].new` — the built-in table never
    /// produces it.
    New,
    /// Forward activity that favours neither tier.
    Other,
}

/// Whether a `Mutate` call edits in place or writes whole files. The scorer
/// counts the two separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutateKind {
    Edit,
    Write,
}

/// Classify one tool by its exact Claude Code name, ASCII case-insensitively,
/// against the built-in table alone.
///
/// This is the table operator config may *not* override: config validation
/// rejects a `[models.router.tool_semantics]` entry naming anything this
/// returns Observe, Mutate, or Plan for, so the two callers agree by
/// construction rather than by a precedence rule that could drift.
///
/// `Bash` is deliberately `Other` rather than parsed: `git status` observes,
/// `rm -rf` mutates, and `cargo test` is a settled-state signal, so only
/// `input.command` could tell them apart. Classifying command strings is a new
/// fingerprinting surface with its own failure mode, and leaving `Bash`
/// unclassified instead feeds `pure_bash_streak`, which the scorer already reads
/// as "trailing activity of unknown character". Operators who want it counted
/// can say so per deployment once `tool_semantics` is wired.
///
/// `Skill` is `Other` for the same reason. It names a packaged set of
/// instructions, not an activity: one skill plans, the next edits files, and the
/// call site records only the skill's name. Reading it as `Plan` would count
/// arbitrary work as investigation on the strength of the dispatcher alone,
/// which is the guess `Bash` is already refused.
pub(crate) fn classify_builtin(name: &str) -> ToolCategory {
    if name.eq_ignore_ascii_case("Read")
        || name.eq_ignore_ascii_case("Glob")
        || name.eq_ignore_ascii_case("Grep")
        || name.eq_ignore_ascii_case("NotebookRead")
        || name.eq_ignore_ascii_case("WebFetch")
        || name.eq_ignore_ascii_case("WebSearch")
        || name.eq_ignore_ascii_case("BashOutput")
    {
        return ToolCategory::Observe;
    }
    if mutate_kind(name).is_some() {
        return ToolCategory::Mutate;
    }
    if name.eq_ignore_ascii_case("TodoWrite")
        || name.eq_ignore_ascii_case("Task")
        || name.eq_ignore_ascii_case("EnterPlanMode")
        || name.eq_ignore_ascii_case("ExitPlanMode")
    {
        return ToolCategory::Plan;
    }
    // Everything else, `Bash`, `Skill`, `KillShell`, and every `mcp__*` server
    // tool included. An unknown name is forward activity of unknown character,
    // not evidence for either tier — unless the operator says otherwise, which
    // is what [`classify`] layers on top.
    ToolCategory::Other
}

/// Classify one tool against the built-in table, falling back to the operator's
/// `[models.router.tool_semantics]` lists.
///
/// Additive, never overriding: a built-in Observe/Mutate/Plan name is returned
/// as-is and the operator's lists are not consulted for it. That ordering is
/// upstream's (`classify_tool_call_with_semantics`) and is enforced a second
/// time at load, where such a name is a `ConfigError` rather than a silently
/// ignored line.
pub(crate) fn classify(name: &str, semantics: &ToolSemanticsConfig) -> ToolCategory {
    let builtin = classify_builtin(name);
    if builtin != ToolCategory::Other {
        return builtin;
    }
    for (category, names) in semantics.categories() {
        if names.iter().any(|listed| listed.eq_ignore_ascii_case(name)) {
            return match category {
                "observe" => ToolCategory::Observe,
                "mutate" => ToolCategory::Mutate,
                "plan" => ToolCategory::Plan,
                _ => ToolCategory::New,
            };
        }
    }
    ToolCategory::Other
}

/// The `Mutate` sub-kind for a tool name, or `None` when it does not mutate.
///
/// Built-in names only. An operator-declared `mutate` name has no sub-kind
/// here and is counted as a [`MutateKind::Write`] by the extractor: shunt
/// cannot see inside a tool it did not name, so whole-file semantics is the
/// conservative reading — and it is the one upstream's `ToolSemantics::classify`
/// takes, for the same reason.
pub(crate) fn mutate_kind(name: &str) -> Option<MutateKind> {
    if name.eq_ignore_ascii_case("Edit") || name.eq_ignore_ascii_case("NotebookEdit") {
        Some(MutateKind::Edit)
    } else if name.eq_ignore_ascii_case("Write") {
        Some(MutateKind::Write)
    } else {
        None
    }
}

/// Whether a failed call from this tool means a whole delegated run collapsed
/// rather than one bad path. A `Task` failure is a subagent's entire run, so it
/// is the strongest single error signal Claude Code can emit.
pub(crate) fn is_delegated_run(name: &str) -> bool {
    name.eq_ignore_ascii_case("Task")
}
