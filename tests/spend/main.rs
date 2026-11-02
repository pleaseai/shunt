//! Spend enforcement, metering, rate-limit headers, and persistence tests.
//!
//! One binary rather than one per file because every integration test binary
//! statically links the whole dependency graph, and that link is the cost.

#[path = "../common/mod.rs"]
mod common;
#[path = "../judge_harness/mod.rs"]
mod judge_harness;

mod spend_enforcement;
mod spend_headers;
mod spend_metering;
mod spend_metering_routing;
mod spend_persistence;
