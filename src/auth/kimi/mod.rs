//! Kimi Code subscription OAuth: device-code login, refresh, and named
//! account storage. See [`auth::KimiAuthStore`] for the wire contract and
//! [`store`] for the on-disk account shape.

pub mod auth;
pub mod login;
pub mod store;
