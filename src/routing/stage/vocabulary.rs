//! Claude Code's tool names, mapped onto the stage router's activity categories.
//!
//! Switchyard ships a provider-neutral vocabulary because it cannot know which
//! agent is calling. shunt can: the inbound protocol is Claude Code's, and its
//! tool names are a stable part of that contract. Naming them directly is the
//! one place this router can be more accurate than the generic algorithm.

/// What one tool call says about where the session is in its work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolCategory {
    /// Read-only lookup or inspection. Investigative, non-producing.
    Observe,
    /// Changes file or task state. Production.
    Mutate,
    /// Explicit planning or delegation. Investigative, non-producing.
    Plan,
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

/// Classify one tool by its exact Claude Code name, ASCII case-insensitively.
///
/// `Bash` is deliberately `Other` rather than parsed: `git status` observes,
/// `rm -rf` mutates, and `cargo test` is a settled-state signal, so only
/// `input.command` could tell them apart. Classifying command strings is a new
/// fingerprinting surface with its own failure mode, and leaving `Bash`
/// unclassified instead feeds `pure_bash_streak`, which the scorer already reads
/// as "trailing activity of unknown character". Operators who want it counted
/// can say so per deployment once `tool_semantics` is wired.
pub(crate) fn classify(name: &str) -> ToolCategory {
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
        || name.eq_ignore_ascii_case("Skill")
        || name.eq_ignore_ascii_case("ExitPlanMode")
    {
        return ToolCategory::Plan;
    }
    // Everything else, `Bash`, `KillShell`, and every `mcp__*` server tool
    // included. An unknown name is forward activity of unknown character, not
    // evidence for either tier.
    ToolCategory::Other
}

/// The `Mutate` sub-kind for a tool name, or `None` when it does not mutate.
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
