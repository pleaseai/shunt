//! xAI (Grok) account-store metadata for the admin web surface.
//!
//! Read-only: xAI accounts are added with `shunt login xai --name <name>`, not
//! from the browser. The listing exists so the dashboard can fold the Grok CLI
//! login into the pooled account of the same xAI user.

use axum::{extract::State, http::HeaderMap, response::Response};
use serde_json::json;

use crate::{auth::xai::store as xai_store, server::AppState};

use super::{authenticate, internal, json_secure, unauthorized};

pub(super) async fn list_xai_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let state = state.refreshed();
    if authenticate(&state, &headers).is_none() {
        return unauthorized();
    }
    match tokio::task::spawn_blocking(xai_store::list_account_meta).await {
        Ok(Ok(accounts)) => json_secure(json!({ "accounts": accounts })),
        Ok(Err(error)) => {
            tracing::error!(%error, "admin: failed to list xAI account metadata");
            internal("failed to list accounts")
        }
        Err(join_error) => {
            tracing::error!(%join_error, "admin: xAI list_account_meta task panicked");
            internal("failed to list accounts")
        }
    }
}
