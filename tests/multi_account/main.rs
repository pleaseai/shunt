//! Multi-account pool tests, one module per provider.
//!
//! One binary rather than one per file because every integration test binary
//! statically links the whole dependency graph, and that link is the cost.

#[path = "../common/mod.rs"]
mod common;

mod antigravity_multi_account;
mod chatgpt_plan_multi_account;
mod codex_multi_account;
mod kimi_multi_account;
mod multi_account;
mod xai_multi_account;
