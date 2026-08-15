//! Spend-limit admin API (`[server.spend]`).
//!
//! Registered independently of `[server.gateway]`: the routes authenticate
//! with the `[server.admin]` credential, so a deployment that never serves
//! gateway login can still administer spend limits.

pub mod api;
pub mod persist;
pub mod store;

use axum::{routing::get, Router};

use crate::server::AppState;

pub use store::SpendStore;

/// The spend-limit route tree, merged into the main router only when
/// `[server.spend]` is configured.
pub fn spend_router() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/organizations/spend_limits",
            get(api::list)
                .post(api::create)
                .fallback(api::method_not_allowed),
        )
        .route(
            "/v1/organizations/spend_limits/{id}",
            get(api::get_by_id)
                .delete(api::delete_by_id)
                .fallback(api::method_not_allowed),
        )
}

#[cfg(test)]
mod tests;
