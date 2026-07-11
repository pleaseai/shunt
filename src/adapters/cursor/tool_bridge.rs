//! Cursor tool bridge: SSE pause state machine and pending tool tracking.
//!
//! The bridge coordinates the pause-and-continue lifecycle when the Cursor
//! upstream emits a `<tool_use>` text block. The bridge pauses the SSE stream,
//! stores the pending tool, and waits for Claude's `tool_result` in the next
//! client request. The resume request is re-run against the upstream agent (in
//! `cursor::forward`) with the tool result carried in the prompt history, so no
//! stored upstream events are replayed.

use std::collections::BTreeSet;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::adapters::cursor::response::CursorStreamEvent;
use crate::adapters::cursor::sse::CursorSseFramer;
use crate::adapters::cursor::tool_use_xml::{CursorToolUseXmlParser, RecoveredCursorEvent};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A pending Cursor tool awaiting the client's `tool_result`.
///
/// Only the tool_use id is retained: it lets the next request be recognized as a
/// resume (via `find_tool_result`). The tool's arguments are not stored — on
/// resume the upstream agent is re-run with the full prompt history, which
/// already carries both the `tool_use` and its `tool_result`.
#[derive(Debug, Clone)]
pub struct PendingCursorTool {
    pub tool_use_id: String,
}

impl PendingCursorTool {
    pub fn tool_use_id(&self) -> &str {
        &self.tool_use_id
    }
}

/// Bridge state stored per session.
#[derive(Debug)]
pub struct CursorBridgeState {
    pub session_id: String,
    pub pending_tool: Option<PendingCursorTool>,
    pub allowed_tool_names: Option<BTreeSet<String>>,
    pub xml_parser: CursorToolUseXmlParser,
    /// When this session was inserted. Used to evict abandoned sessions — a
    /// client that pauses on a tool_use but never resumes would otherwise leak
    /// its state forever. Eviction is safe: a resume whose state was dropped
    /// falls through to a fresh upstream run in `cursor::forward`, which still
    /// carries the tool_result in the prompt history.
    pub created_at: Instant,
}

impl CursorBridgeState {
    fn new(
        session_id: String,
        allowed_tool_names: Option<BTreeSet<String>>,
        id_factory: Box<dyn FnMut() -> String + Send>,
    ) -> Self {
        Self {
            session_id,
            pending_tool: None,
            allowed_tool_names: allowed_tool_names.clone(),
            xml_parser: CursorToolUseXmlParser::new_with_id_factory(allowed_tool_names, id_factory),
            created_at: Instant::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Global bridge registry
// ---------------------------------------------------------------------------

/// Abandoned bridge sessions are evicted after this idle window. A paused
/// tool_use normally resumes within seconds (the client executes one tool), so
/// 30 minutes is generously above the legitimate window while still bounding
/// memory for sessions that never resume.
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// Hard cap on retained sessions as a backstop against a burst of new sessions
/// within the TTL window. When exceeded, the oldest session is dropped.
const MAX_SESSIONS: usize = 1024;

static BRIDGE_REGISTRY: LazyLock<Mutex<BridgeRegistryInner>> =
    LazyLock::new(|| Mutex::new(BridgeRegistryInner::new()));

struct BridgeRegistryInner {
    sessions: Vec<CursorBridgeState>,
}

impl BridgeRegistryInner {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
        }
    }
}

/// Global registry of active bridge sessions.
pub struct BridgeRegistry;

impl BridgeRegistry {
    /// Insert a new bridge state for a session.
    ///
    /// Removes any existing state with the same `session_id` first so a client
    /// starting a fresh request (rather than resuming) does not accumulate stale
    /// duplicate sessions in the registry.
    pub fn insert(state: CursorBridgeState) {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.retain(|s| s.session_id != state.session_id);
        // Opportunistically evict abandoned sessions (idle past the TTL) so a
        // client that pauses but never resumes does not leak state indefinitely.
        // saturating_duration_since (not duration_since) so a clock quirk that
        // makes created_at appear to be in the future yields 0 instead of panicking.
        let now = Instant::now();
        reg.sessions
            .retain(|s| now.saturating_duration_since(s.created_at) < SESSION_TTL);
        // Backstop: if a burst of live sessions still exceeds the cap, drop the
        // oldest. `take`/`swap_remove` break insertion order, so find the min.
        while reg.sessions.len() >= MAX_SESSIONS {
            if let Some(oldest) = reg
                .sessions
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.created_at)
                .map(|(index, _)| index)
            {
                reg.sessions.swap_remove(oldest);
            } else {
                break;
            }
        }
        reg.sessions.push(state);
    }

    /// Get the bridge state for a session.
    pub fn get(session_id: &str) -> Option<usize> {
        let reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.iter().position(|s| s.session_id == session_id)
    }

    /// Get the pending tool for a session (if any).
    pub fn pending_tool(session_id: &str) -> Option<PendingCursorTool> {
        let reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .and_then(|s| s.pending_tool.clone())
    }

    /// Take the bridge state for a session (removes it).
    pub fn take(session_id: &str) -> Option<CursorBridgeState> {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        let pos = reg
            .sessions
            .iter()
            .position(|s| s.session_id == session_id)?;
        Some(reg.sessions.swap_remove(pos))
    }

    /// Remove a bridge state for a session.
    pub fn remove(session_id: &str) {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.retain(|s| s.session_id != session_id);
    }

    /// Insert or update the pending tool for a session.
    pub fn set_pending_tool(session_id: &str, tool: PendingCursorTool) {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        if let Some(state) = reg.sessions.iter_mut().find(|s| s.session_id == session_id) {
            state.pending_tool = Some(tool);
        }
    }

    /// Clear all bridge state.
    pub fn clear() {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.clear();
    }

    /// Number of active sessions.
    pub fn active_count() -> usize {
        let reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.len()
    }
}

// ---------------------------------------------------------------------------
// Tool detection helpers
// ---------------------------------------------------------------------------

/// Extract advertised tool names from a MessagesRequest.
pub fn advertised_tool_names(body: &Value) -> Option<BTreeSet<String>> {
    let tools = body.get("tools")?.as_array()?;
    if tools.is_empty() {
        return None;
    }
    let names: BTreeSet<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .map(|n| n.to_string())
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

/// Whether the request can use the Cursor native tool bridge.
///
/// Returns `true` when the request is streaming, has a session id, and
/// advertises at least one of Read, Write, or Bash.
pub fn can_bridge_cursor_native_tools(body: &Value, session_id: Option<&str>) -> bool {
    let _sid = match session_id {
        Some(id) if !id.is_empty() => id,
        _ => return false,
    };
    if !body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        return false;
    }
    let names = match advertised_tool_names(body) {
        Some(n) => n,
        None => return false,
    };
    names.contains("Read") || names.contains("Write") || names.contains("Bash")
}

// ---------------------------------------------------------------------------
// Result helpers
// ---------------------------------------------------------------------------

/// Find the last `tool_result` block matching `tool_use_id` in the request.
pub fn find_tool_result<'a>(body: &'a Value, tool_use_id: &str) -> Option<&'a serde_json::Value> {
    for message in body.get("messages").and_then(Value::as_array)?.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let blocks = match message.get("content") {
            Some(serde_json::Value::Array(arr)) => arr,
            _ => continue,
        };
        for block in blocks.iter().rev() {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                && block.get("tool_use_id").and_then(|t| t.as_str()) == Some(tool_use_id)
            {
                return Some(block);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Bridge start
// ---------------------------------------------------------------------------

/// Start a new tool bridge session.
///
/// Processes upstream events through XML recovery. When a `<tool_use>` is
/// recovered, emits the SSE pause (tool_use content block + message_stop with
/// stop_reason="tool_use") and stores the bridge state for resume.
///
/// Returns the SSE bytes and whether a tool_use pause was emitted.
pub fn start_cursor_tool_bridge(
    message_id: &str,
    model: &str,
    session_id: &str,
    events: &[CursorStreamEvent],
    allowed_tool_names: Option<BTreeSet<String>>,
    id_factory: Box<dyn FnMut() -> String + Send>,
) -> (Vec<u8>, bool) {
    let mut framer = CursorSseFramer::new(message_id, model);

    let mut state = CursorBridgeState::new(session_id.to_string(), allowed_tool_names, id_factory);

    let mut paused = false;

    for event in events {
        if paused {
            // Already paused at a tool_use. The rest of this pre-generated stream
            // is discarded — on resume the upstream agent is re-run with the tool
            // result, rather than replaying these stale events.
            continue;
        }

        match event {
            CursorStreamEvent::ThinkingDelta { text } => {
                framer.emit_thinking_delta(text);
            }
            CursorStreamEvent::TextDelta { text } => {
                let recovered = state.xml_parser.push(text);
                for recovered_event in &recovered {
                    if paused {
                        continue;
                    }
                    match recovered_event {
                        RecoveredCursorEvent::Text(t) => {
                            framer.emit_text_delta(t);
                        }
                        RecoveredCursorEvent::ToolUse(tool_use) => {
                            let input_json = serde_json::to_string(&tool_use.input)
                                .unwrap_or_else(|_| "{}".to_string());
                            framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);

                            if let Some(pending) = pending_from_recovered_tool(tool_use) {
                                state.pending_tool = Some(pending);
                            }

                            paused = true;
                        }
                    }
                }
            }
            CursorStreamEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                framer.record_usage(*input_tokens, *output_tokens, 0, 0);
            }
            CursorStreamEvent::Session { .. } => {
                // Session info is not mapped to SSE events
            }
            CursorStreamEvent::End => {
                // If we haven't paused, finalize normally
                if !paused {
                    // Process any remaining XML before finalizing
                    let flushed = state.xml_parser.flush();
                    for evt in &flushed {
                        if let RecoveredCursorEvent::ToolUse(tool_use) = evt {
                            let input_json = serde_json::to_string(&tool_use.input)
                                .unwrap_or_else(|_| "{}".to_string());
                            framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
                            if let Some(pending) = pending_from_recovered_tool(tool_use) {
                                state.pending_tool = Some(pending);
                            }
                            paused = true;
                        }
                    }
                    if !paused {
                        framer.finalize();
                    }
                }
            }
        }
    }

    if !paused {
        // Flush any remaining text from XML parser
        let flushed = state.xml_parser.flush();
        for evt in &flushed {
            if let RecoveredCursorEvent::ToolUse(tool_use) = evt {
                let input_json =
                    serde_json::to_string(&tool_use.input).unwrap_or_else(|_| "{}".to_string());
                framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
                if let Some(pending) = pending_from_recovered_tool(tool_use) {
                    state.pending_tool = Some(pending);
                }
                paused = true;
            }
        }
        if !paused {
            framer.finalize();
        }
    }

    // If a native tool_use paused the stream, store minimal state so the next
    // request is recognized as a resume (its tool_use id is matched against the
    // incoming tool_result). No upstream events are stored — resume re-runs the
    // agent with the tool result carried in the prompt history.
    if let Some(pending) = state.pending_tool.clone() {
        let mut stored_state = CursorBridgeState::new(
            session_id.to_string(),
            state.allowed_tool_names.clone(),
            Box::new(|| {
                format!(
                    "call_cursor_{}",
                    uuid::Uuid::new_v4().to_string().replace('-', "")
                )
            }),
        );
        stored_state.pending_tool = Some(pending);
        BridgeRegistry::insert(stored_state);
    }

    (framer.take_output(), paused)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Create a `PendingCursorTool` from a recovered XML tool_use event.
///
/// Returns `Some` only for the natively-bridged tools (Read / Write / Bash);
/// only the tool_use id is captured, since resume re-runs the upstream agent
/// with the tool result carried in the prompt history.
fn pending_from_recovered_tool(
    tool_use: &crate::adapters::cursor::tool_use_xml::RecoveredCursorToolUse,
) -> Option<PendingCursorTool> {
    match tool_use.name.as_str() {
        "Read" | "Write" | "Bash" => Some(PendingCursorTool {
            tool_use_id: tool_use.id.clone(),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize tests that share the global bridge registry.
    static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    // -----------------------------------------------------------------------
    // find_tool_result tests
    // -----------------------------------------------------------------------

    #[test]
    fn finds_tool_result_in_request() {
        let body: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "result text"}]}
            ]
        });
        let result = find_tool_result(&body, "call_1");
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().get("content").and_then(|c| c.as_str()),
            Some("result text")
        );
    }

    #[test]
    fn find_tool_result_returns_none_when_not_found() {
        let body: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]}
            ]
        });
        assert!(find_tool_result(&body, "call_1").is_none());
    }

    #[test]
    fn find_tool_result_scans_newest_first() {
        let body: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "old"}]},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "new"}]}
            ]
        });
        let result = find_tool_result(&body, "call_1");
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().get("content").and_then(|c| c.as_str()),
            Some("new")
        );
    }

    // -----------------------------------------------------------------------
    // advertised_tool_names tests
    // -----------------------------------------------------------------------

    #[test]
    fn advertised_tool_names_extracts_read_write_bash() {
        let body: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "Read", "description": "read", "input_schema": {}},
                {"name": "Write", "description": "write", "input_schema": {}},
                {"name": "Bash", "description": "bash", "input_schema": {}}
            ]
        });
        let names = advertised_tool_names(&body).unwrap();
        assert!(names.contains("Read"));
        assert!(names.contains("Write"));
        assert!(names.contains("Bash"));
    }

    #[test]
    fn advertised_tool_names_no_tools_returns_none() {
        let body: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert!(advertised_tool_names(&body).is_none());
    }

    #[test]
    fn can_bridge_returns_true_for_stream_with_read_tool() {
        let body: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}]
        });
        assert!(can_bridge_cursor_native_tools(&body, Some("session-1")));
    }

    #[test]
    fn can_bridge_returns_false_for_non_streaming() {
        let body: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": false,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}]
        });
        assert!(!can_bridge_cursor_native_tools(&body, Some("session-1")));
    }

    #[test]
    fn can_bridge_returns_false_without_session_id() {
        let body: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read", "input_schema": {}}]
        });
        assert!(!can_bridge_cursor_native_tools(&body, None));
        assert!(!can_bridge_cursor_native_tools(&body, Some("")));
    }

    // -----------------------------------------------------------------------
    // BridgeRegistry tests
    // -----------------------------------------------------------------------

    #[test]
    fn bridge_registry_manages_sessions() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();
        assert_eq!(BridgeRegistry::active_count(), 0);

        let state = CursorBridgeState::new("session-test".into(), None, Box::new(|| "id".into()));
        BridgeRegistry::insert(state);
        assert_eq!(BridgeRegistry::active_count(), 1);
        assert!(BridgeRegistry::get("session-test").is_some());

        let state = BridgeRegistry::take("session-test");
        assert!(state.is_some());
        assert_eq!(BridgeRegistry::active_count(), 0);
    }

    #[test]
    fn insert_evicts_sessions_past_ttl() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();

        // Build a timestamp older than the TTL. On a host booted less than the
        // TTL ago the monotonic clock can't represent it — skip rather than
        // assert against an impossible precondition.
        let Some(stale_instant) = Instant::now().checked_sub(SESSION_TTL + Duration::from_secs(1))
        else {
            return;
        };

        let mut stale =
            CursorBridgeState::new("stale-session".into(), None, Box::new(|| "id".into()));
        stale.created_at = stale_instant;
        BridgeRegistry::insert(stale);
        assert_eq!(BridgeRegistry::active_count(), 1);

        // Inserting a fresh session opportunistically evicts the abandoned one.
        let fresh = CursorBridgeState::new("fresh-session".into(), None, Box::new(|| "id".into()));
        BridgeRegistry::insert(fresh);

        assert_eq!(BridgeRegistry::active_count(), 1);
        assert!(BridgeRegistry::get("stale-session").is_none());
        assert!(BridgeRegistry::get("fresh-session").is_some());

        BridgeRegistry::clear();
    }

    #[test]
    fn bridge_registry_set_and_get_pending_tool() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();
        let state = CursorBridgeState::new("session-pt".into(), None, Box::new(|| "id".into()));
        BridgeRegistry::insert(state);

        let tool = PendingCursorTool {
            tool_use_id: "call_1".into(),
        };
        BridgeRegistry::set_pending_tool("session-pt", tool);

        let retrieved = BridgeRegistry::pending_tool("session-pt");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().tool_use_id(), "call_1");

        BridgeRegistry::clear();
    }

    #[test]
    fn start_bridge_pauses_and_stores_only_pending_tool_id() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();

        // A recovered Read tool_use should pause the stream and store just the
        // tool_use id for resume detection.
        let events = vec![
            CursorStreamEvent::TextDelta {
                text: r#"<tool_use name="Read">{"file_path":"/tmp/a"}</tool_use>"#.into(),
            },
            CursorStreamEvent::End,
        ];
        let allowed = Some(["Read".to_string()].into_iter().collect());
        let (_sse, paused) = start_cursor_tool_bridge(
            "msg-1",
            "cursor-test",
            "sess-resume",
            &events,
            allowed,
            Box::new(|| "call_1".into()),
        );
        assert!(paused);

        let pending = BridgeRegistry::pending_tool("sess-resume").expect("pending stored");
        assert_eq!(pending.tool_use_id(), "call_1");

        // The resume path (see `cursor::forward`) drops the stored state before
        // re-running the upstream agent; nothing is replayed.
        BridgeRegistry::remove("sess-resume");
        assert!(BridgeRegistry::pending_tool("sess-resume").is_none());

        BridgeRegistry::clear();
    }
}
