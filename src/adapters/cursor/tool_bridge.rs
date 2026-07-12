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

/// Transient state used only while processing one upstream response. It is not
/// stored in the registry — only the resulting pending tool is (see
/// [`ActiveSession`]). Holds the XML recovery parser and the tool (if any) that
/// paused the stream.
#[derive(Debug)]
pub struct CursorBridgeState {
    pub pending_tool: Option<PendingCursorTool>,
    pub xml_parser: CursorToolUseXmlParser,
}

impl CursorBridgeState {
    fn new(
        allowed_tool_names: Option<BTreeSet<String>>,
        id_factory: Box<dyn FnMut() -> String + Send>,
    ) -> Self {
        Self {
            pending_tool: None,
            xml_parser: CursorToolUseXmlParser::new_with_id_factory(allowed_tool_names, id_factory),
        }
    }
}

// ---------------------------------------------------------------------------
// Global bridge registry
// ---------------------------------------------------------------------------

/// A paused bridge session awaiting the client's `tool_result`. Only what resume
/// needs is retained — the pending tool's id and the creation time for eviction
/// — not the heavyweight `xml_parser`/`id_factory` used to produce it.
#[derive(Debug, Clone)]
struct ActiveSession {
    session_id: String,
    pending_tool: PendingCursorTool,
    /// When this session was inserted. Abandoned sessions (paused on a tool_use
    /// but never resumed) would otherwise leak forever. Eviction is safe: a
    /// resume whose state was dropped falls through to a fresh upstream run in
    /// `cursor::forward`, which still carries the tool_result in prompt history.
    created_at: Instant,
}

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
    sessions: Vec<ActiveSession>,
}

impl BridgeRegistryInner {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
        }
    }
}

/// Global registry of paused bridge sessions.
pub struct BridgeRegistry;

impl BridgeRegistry {
    /// Record a paused session's pending tool, keyed by `session_id`.
    ///
    /// Removes any existing entry for the session first so a client starting a
    /// fresh request (rather than resuming) does not accumulate duplicates, then
    /// evicts abandoned sessions before pushing the new one.
    pub fn insert(session_id: &str, pending_tool: PendingCursorTool) {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.retain(|s| s.session_id != session_id);
        Self::evict_locked(&mut reg);
        reg.sessions.push(ActiveSession {
            session_id: session_id.to_string(),
            pending_tool,
            created_at: Instant::now(),
        });
    }

    /// Evict sessions idle past the TTL and, as a backstop, the oldest sessions
    /// once the hard cap is reached.
    fn evict_locked(reg: &mut BridgeRegistryInner) {
        // saturating_duration_since (not duration_since) so a clock quirk that
        // makes created_at appear to be in the future yields 0 instead of panicking.
        let now = Instant::now();
        reg.sessions
            .retain(|s| now.saturating_duration_since(s.created_at) < SESSION_TTL);
        // swap_remove breaks insertion order, so find the actual oldest.
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
    }

    /// Get the pending tool for a session (if any).
    pub fn pending_tool(session_id: &str) -> Option<PendingCursorTool> {
        let reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .map(|s| s.pending_tool.clone())
    }

    /// Remove a session from the registry.
    pub fn remove(session_id: &str) {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.retain(|s| s.session_id != session_id);
    }

    /// Whether a session is currently registered.
    #[cfg(test)]
    pub fn contains(session_id: &str) -> bool {
        let reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.iter().any(|s| s.session_id == session_id)
    }

    /// Insert a session with an explicit creation time, bypassing eviction, so
    /// tests can seed a stale entry.
    #[cfg(test)]
    fn insert_at(session_id: &str, pending_tool: PendingCursorTool, created_at: Instant) {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.retain(|s| s.session_id != session_id);
        reg.sessions.push(ActiveSession {
            session_id: session_id.to_string(),
            pending_tool,
            created_at,
        });
    }

    /// Clear all bridge state.
    #[cfg(test)]
    pub fn clear() {
        let mut reg = BRIDGE_REGISTRY.lock().unwrap();
        reg.sessions.clear();
    }

    /// Number of active sessions.
    #[cfg(test)]
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
    // Buffered path: seed usage before the first emit so message_start carries
    // the real input-token count instead of the placeholder 1.
    framer.preseed_usage(events);

    let mut state = CursorBridgeState::new(allowed_tool_names, id_factory);

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
                cache_read_tokens,
                cache_write_tokens,
            } => {
                framer.record_usage(
                    *input_tokens,
                    *output_tokens,
                    *cache_read_tokens,
                    *cache_write_tokens,
                );
            }
            CursorStreamEvent::Session { .. } => {
                // Session info is not mapped to SSE events
            }
            // The final XML flush and finalize happen once after the loop, so
            // End needs no per-event handling (this also avoids a double flush
            // when the upstream sends End before the stream truly ends).
            CursorStreamEvent::End => {}
        }
    }

    if !paused {
        // Flush any remaining text from XML parser
        let flushed = state.xml_parser.flush();
        for evt in &flushed {
            // Once a tool_use pauses the stream, stop: emitting further events
            // after the pause would append deltas past the SSE message_stop. The
            // remaining events are handled when the upstream agent re-runs on resume.
            if paused {
                break;
            }
            match evt {
                // Trailing text the parser held back must still reach the client.
                RecoveredCursorEvent::Text(text) => {
                    framer.emit_text_delta(text);
                }
                RecoveredCursorEvent::ToolUse(tool_use) => {
                    let input_json =
                        serde_json::to_string(&tool_use.input).unwrap_or_else(|_| "{}".to_string());
                    framer.emit_tool_pause(&tool_use.id, &tool_use.name, &input_json);
                    if let Some(pending) = pending_from_recovered_tool(tool_use) {
                        state.pending_tool = Some(pending);
                    }
                    paused = true;
                }
            }
        }
        if !paused {
            framer.finalize();
        }
    }

    // If a native tool_use paused the stream, store the pending tool id so the
    // next request is recognized as a resume (matched against the incoming
    // tool_result). No upstream events are stored — resume re-runs the agent
    // with the tool result carried in the prompt history.
    if let Some(pending) = state.pending_tool {
        BridgeRegistry::insert(session_id, pending);
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

    fn pending(id: &str) -> PendingCursorTool {
        PendingCursorTool {
            tool_use_id: id.into(),
        }
    }

    #[test]
    fn bridge_registry_manages_sessions() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();
        assert_eq!(BridgeRegistry::active_count(), 0);

        BridgeRegistry::insert("session-test", pending("call_1"));
        assert_eq!(BridgeRegistry::active_count(), 1);
        assert!(BridgeRegistry::contains("session-test"));

        BridgeRegistry::remove("session-test");
        assert!(!BridgeRegistry::contains("session-test"));
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

        BridgeRegistry::insert_at("stale-session", pending("call_stale"), stale_instant);
        assert_eq!(BridgeRegistry::active_count(), 1);

        // Inserting a fresh session opportunistically evicts the abandoned one.
        BridgeRegistry::insert("fresh-session", pending("call_fresh"));

        assert_eq!(BridgeRegistry::active_count(), 1);
        assert!(!BridgeRegistry::contains("stale-session"));
        assert!(BridgeRegistry::contains("fresh-session"));

        BridgeRegistry::clear();
    }

    #[test]
    fn bridge_registry_stores_and_reads_pending_tool() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();

        BridgeRegistry::insert("session-pt", pending("call_1"));

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

    #[test]
    fn trailing_held_text_is_flushed_not_dropped() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();

        // An incomplete tool_use tag never closes: the parser emits the leading
        // text, then holds the partial tag. It must be flushed as text at stream
        // end rather than silently dropped, and the stream must not pause.
        let events = vec![
            CursorStreamEvent::TextDelta {
                text: r#"some text <tool_use name="Read">{"a":1}"#.into(),
            },
            CursorStreamEvent::End,
        ];
        let allowed = Some(["Read".to_string()].into_iter().collect());
        let (sse, paused) = start_cursor_tool_bridge(
            "msg-flush",
            "cursor-test",
            "sess-flush",
            &events,
            allowed,
            Box::new(|| "call_x".into()),
        );
        assert!(
            !paused,
            "an unclosed tool_use tag must not pause the stream"
        );
        let sse = String::from_utf8(sse).unwrap();
        assert!(sse.contains("some text"), "leading text missing: {sse}");
        assert!(
            sse.contains("tool_use"),
            "held partial-tag text was dropped: {sse}"
        );

        BridgeRegistry::clear();
    }

    #[test]
    fn bridge_preserves_cache_usage_in_message_delta() {
        let _lock = REGISTRY_LOCK.lock().unwrap();
        BridgeRegistry::clear();

        // A plain-text (non-paused) stream carrying cache usage must surface the
        // cache counts, not overwrite them with zeros in the emit loop.
        let events = vec![
            CursorStreamEvent::TextDelta {
                text: "hello".into(),
            },
            CursorStreamEvent::Usage {
                input_tokens: 20,
                output_tokens: 5,
                cache_read_tokens: 7,
                cache_write_tokens: 9,
            },
            CursorStreamEvent::End,
        ];
        let (sse, paused) = start_cursor_tool_bridge(
            "msg-cache",
            "cursor-test",
            "sess-cache",
            &events,
            None,
            Box::new(|| "call_c".into()),
        );
        assert!(!paused);
        let sse = String::from_utf8(sse).unwrap();
        // The final message_delta must carry the real cache counts.
        assert!(
            sse.contains("\"cache_creation_input_tokens\":9"),
            "missing cache_creation: {sse}"
        );
        assert!(
            sse.contains("\"cache_read_input_tokens\":7"),
            "missing cache_read: {sse}"
        );

        BridgeRegistry::clear();
    }
}
