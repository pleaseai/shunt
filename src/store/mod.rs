//! Optional pluggable store (ADR-0006): SQLite always, PostgreSQL behind `store-postgres`.

pub mod boot;
pub mod config;
pub mod db;
pub mod error;
pub mod hold;
pub mod import;
pub mod inspect;
pub(crate) mod marker;
pub mod migrate;
pub mod open;
pub(crate) mod pool_repo;
pub(crate) mod sessions_repo;
pub(crate) mod spend_repo;

pub use config::{StoreBackend, StoreConfig};
pub use db::{rewrite_placeholders, Db, Row, Tx, Value};
pub use error::{to_i64, StoreError};
pub use hold::HeartbeatHandle;
pub use open::{Mode, Store};

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
pub(crate) mod contract;
