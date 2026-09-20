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
    /// The client's Anthropic `stop_sequences`, emulated gateway-side because
    /// the Responses API has no `stop` parameter (issue #605). Usually empty.
    pub stop_sequences: Vec<String>,
}

impl RelayOptions {
    /// Build the SSE translation machine these options describe. Consumes `self`
    /// so the model name moves into the machine without a second clone (the name
    /// was already cloned once from the [`Route`] in [`TurnOptions::relay`], and
    /// no relay call site touches the options after building the machine).
    pub(super) fn machine(self) -> AnthropicSseMachine {
        AnthropicSseMachine::new(self.model, self.thinking_enabled, self.tool_search_native)
            .with_stop_sequences(self.stop_sequences)
    }
}

/// The request-derived flags every forward path shares, computed once in
/// `forward` and threaded through each transport. `model` is intentionally
/// absent — it is taken from the [`Route`] at relay time via
/// [`TurnOptions::relay`], keeping these flags transport-agnostic.
#[derive(Debug, Clone)]
pub(super) struct TurnOptions {
    /// The client asked for a streaming (SSE) response.
    pub client_wants_stream: bool,
    /// Surface reasoning/thinking blocks — the client asked for extended thinking.
    pub thinking_enabled: bool,
    /// Native client-executed `tool_search` is enabled for this provider/model.
    pub tool_search_native: bool,
    /// The client's Anthropic `stop_sequences` (issue #605), in request order.
    /// Never forwarded upstream — the Responses API has no `stop` parameter —
    /// but emulated by the SSE translation.
    pub stop_sequences: Vec<String>,
    /// Bound on a whole-body read of the upstream reply, or `None` for a client
    /// turn (which is byte-for-byte what it was).
    ///
    /// Only the non-streaming path buffers a whole reply, so only it consults
    /// this. It matters because an internal `[models.router]` call is forced
    /// non-streaming, which makes that path the one every judge call through a
    /// `kind = "responses"` target takes — and an unbounded read there would
    /// let a judge allocate freely until the deadline instead of failing open
    /// at `judge_max_response_bytes`.
    pub response_byte_cap: Option<usize>,
}

impl TurnOptions {
    /// The response-rendering options for `route`, pairing these turn flags with
    /// the route's client-facing model name.
    pub(super) fn relay(&self, route: &Route) -> RelayOptions {
        RelayOptions {
            model: route.model.clone(),
            thinking_enabled: self.thinking_enabled,
            tool_search_native: self.tool_search_native,
            stop_sequences: self.stop_sequences.clone(),
        }
    }
}

/// The request-derived fields the single-account transports (`forward_http` and
/// `forward_websocket`) need beyond `state`/`route` and their own connection key
/// (`forward_http` also takes `session_id`, `forward_websocket` also takes
/// `pool_key`). `forward` builds one per transport: a pre-first-event websocket
/// failure falls back to HTTP with the same resolved credential.
/// A credential resolved up front or deferred into the committed stream: the
/// early-commit HTTP arm must not wait on a refreshable credential's
/// (possibly networked) refresh before committing — that wait leaves the
/// client with neither response headers nor keepalive pings, and the OAuth
/// refresh is outside `upstream_ttfb_ms`.
pub(super) enum CredentialSource {
    Resolved(Credential),
    Deferred(
        std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Credential, crate::adapters::AdapterError>>
                    + Send,
            >,
        >,
    ),
}

impl std::fmt::Debug for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolved(_) => f.write_str("Resolved(_)"),
            Self::Deferred(_) => f.write_str("Deferred(_)"),
        }
    }
}

#[derive(Debug)]
pub(super) struct ForwardOptions {
    pub upstream_body: Arc<Value>,
    pub auth: AuthMode,
    pub turn: TurnOptions,
    /// Selected Codex pool account whose WebSocket handshake quota headers should
    /// populate the admin dashboard. `None` on the single-account path.
    pub codex_quota_account: Option<AccountConfig>,
    /// The parsed request to seed `message_start`'s `usage.input_tokens` with a
    /// local tiktoken estimate, or `None` on non-streaming / non-tiktoken turns.
    /// Mirrored on the account-pool path by [`PoolForward::estimate_input`].
    pub estimate_input: Option<Arc<Value>>,
    /// A pre-dispatch instant the committed stream's sample clock starts at —
    /// the single-credential fallback after an account scan ran inside the
    /// dispatch — else the stream's first poll.
    pub started_at: Option<std::time::Instant>,
}

/// Everything `forward_chatgpt_oauth` needs beyond `state`/`route`. The account
/// pool resolves a credential per account (rather than carrying one), so its
/// shape differs from [`ForwardOptions`] there — but it shares the same
/// `estimate_input`: `forward` computes it once, gated identically for every
/// auth mode (`client_wants_stream && CountTokens::Tiktoken`), before branching
/// into the pool vs. single-account dispatch.
#[derive(Debug)]
pub(super) struct PoolForward {
    pub pool_key: Option<String>,
    pub session_id: Option<String>,
    pub upstream_body: Arc<Value>,
    pub accounts_config: Vec<AccountConfig>,
    pub turn: TurnOptions,
    /// The parsed request to seed `message_start`'s `usage.input_tokens` with a
    /// local tiktoken estimate, or `None` on non-streaming / non-tiktoken turns.
    /// Mirrors [`ForwardOptions::estimate_input`]; shared across every account
    /// attempt in the pool loop (the translated body does not change per
    /// account), never re-encoded per rotation.
    pub estimate_input: Option<Arc<Value>>,
}
