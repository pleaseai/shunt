//! Translation adapters for the inbound Codex endpoint's routed path
//! (issue #477, follows #436).
//!
//! `[[server.codex_endpoint.routes]]` relays an inbound OpenAI Responses
//! request byte-for-byte to a `kind = "responses"` upstream. These modules let
//! a route target the two other upstream classes shunt already speaks by
//! translating in both directions, keyed on the routed provider's kind:
//!
//! - **Anthropic Messages** (`kind = "anthropic"`): [`messages_request`]
//!   (Responses request → Messages request) and [`messages_stream`] (Messages
//!   SSE / JSON → Responses SSE / JSON). The mirror image of
//!   [`crate::model::responses_request`] / [`crate::model::responses`].
//! - **Chat Completions** (OpenAI-compatible backends that never adopted
//!   Responses): [`chat_request`] and [`chat_stream`].
//!
//! Both pairs emit the Responses SSE surface through one shared
//! [`events::ResponsesEmitter`] so the Codex CLI sees the same event grammar
//! whichever upstream answered. Streaming is preserved: every machine here is
//! fed one upstream event at a time and yields the Responses events that event
//! justifies — nothing waits for the upstream turn to end.

pub mod chat_request;
pub mod chat_stream;
pub mod events;
pub mod messages_request;
pub mod messages_stream;
pub mod reasoning;
