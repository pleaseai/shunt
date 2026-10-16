//! Optional pluggable store (ADR-0006): SQLite always, PostgreSQL behind `store-postgres`.

pub mod config;

pub use config::{StoreBackend, StoreConfig};
