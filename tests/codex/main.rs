//! Codex spend enforcement, metering, and WebSocket transport tests.
//!
//! One binary rather than one per file because every integration test binary
//! statically links the whole dependency graph, and that link is the cost.

#[path = "../common/mod.rs"]
mod common;
#[path = "../judge_harness/mod.rs"]
mod judge_harness;

mod codex_spend_enforcement;
mod codex_spend_metering;
mod codex_spend_websocket;
mod codex_websocket_fallback;
mod codex_websocket_gated;
