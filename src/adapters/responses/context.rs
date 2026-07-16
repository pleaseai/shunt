//! Parameter objects shared across the Responses transports.
//!
//! Several forward/relay functions in this adapter thread the same cluster of
//! request-derived fields — the client-facing `model`, the `thinking_enabled` /
//! `tool_search_native` protocol toggles, the streaming flag, and the
//! `message_start` input-token estimate. Grouping them keeps those functions
//! within the parameter budget and stops positional call sites from transposing
//! the flags. This follows the existing `WsTurnContext` precedent (see
//! `websocket.rs`) of a struct that carries "everything needed to drive a turn".

use std::sync::Arc;

use serde_json::Value;

use crate::{
    auth::Credential,
    config::{AccountConfig, AuthMode},
    model::responses::AnthropicSseMachine,
    routing::Route,
};

/// How to translate an upstream Responses stream back into Anthropic form:
/// exactly `AnthropicSseMachine::new`'s arguments. Passed as one unit through
/// every relay path (streaming SSE and collected JSON) so the model name and the
/// two protocol toggles can never be handed over out of order.
#[derive(Debug)]
pub(super) struct RelayOptions {
    pub model: String,
    pub thinking_enabled: bool,
    pub tool_search_native: bool,
}

impl RelayOptions {
    /// Build the SSE translation machine these options describe. Consumes `self`
    /// so the model name moves into the machine without a second clone (the name
    /// was already cloned once from the [`Route`] in [`TurnOptions::relay`], and
    /// no relay call site touches the options after building the machine).
    pub(super) fn machine(self) -> AnthropicSseMachine {
        AnthropicSseMachine::new(self.model, self.thinking_enabled, self.tool_search_native)
    }
}

/// The request-derived flags every forward path shares, computed once in
/// `forward` and threaded through each transport. `model` is intentionally
/// absent — it is taken from the [`Route`] at relay time via
/// [`TurnOptions::relay`], keeping these flags transport-agnostic.
#[derive(Debug, Clone, Copy)]
pub(super) struct TurnOptions {
    /// The client asked for a streaming (SSE) response.
    pub client_wants_stream: bool,
    /// Surface reasoning/thinking blocks — the client asked for extended thinking.
    pub thinking_enabled: bool,
    /// Native client-executed `tool_search` is enabled for this provider/model.
    pub tool_search_native: bool,
}

impl TurnOptions {
    /// The response-rendering options for `route`, pairing these turn flags with
    /// the route's client-facing model name.
    pub(super) fn relay(&self, route: &Route) -> RelayOptions {
        RelayOptions {
            model: route.model.clone(),
            thinking_enabled: self.thinking_enabled,
            tool_search_native: self.tool_search_native,
        }
    }
}

/// The request-derived fields the single-account transports (`forward_http` and
/// `forward_websocket`) need beyond `state`/`route` and their own connection key
/// (`forward_http` also takes `session_id`, `forward_websocket` also takes
/// `pool_key`). Cloned once in `forward` so a pre-first-event websocket failure
/// can fall back to HTTP with the same body/credential — the same fields the
/// previous per-argument clones copied.
#[derive(Debug, Clone)]
pub(super) struct ForwardOptions {
    pub upstream_body: Value,
    pub credential: Credential,
    pub auth: AuthMode,
    pub turn: TurnOptions,
    /// Selected Codex pool account whose WebSocket handshake quota headers should
    /// populate the admin dashboard. `None` on the single-account path.
    pub codex_quota_account: Option<String>,
    /// The parsed request to seed `message_start`'s `usage.input_tokens` with a
    /// local tiktoken estimate, or `None` on non-streaming / non-tiktoken turns.
    /// Single-account only; the account-pool path does not thread it yet.
    pub estimate_input: Option<Arc<Value>>,
}

/// Everything `forward_chatgpt_oauth` needs beyond `state`/`route`. The account
/// pool resolves a credential per account (rather than carrying one) and does not
/// pre-compute an input estimate, so its shape differs from [`ForwardOptions`].
#[derive(Debug)]
pub(super) struct PoolForward {
    pub pool_key: Option<String>,
    pub session_id: Option<String>,
    pub upstream_body: Value,
    pub accounts_config: Vec<AccountConfig>,
    pub turn: TurnOptions,
}
