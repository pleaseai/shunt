//! Inbound authentication and inbound Codex endpoint tests.
//!
//! One binary rather than one per file because every integration test binary
//! statically links the whole dependency graph, and that link is the cost.

#[path = "../common/mod.rs"]
mod common;

mod inbound_auth;
mod inbound_codex_endpoint;
mod inbound_codex_jwt_slots;
mod inbound_codex_routes;
mod inbound_codex_search;
mod inbound_codex_websocket;
