use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::{error::ShuntError, server::AppState};

/// Builtin catalog mirrored from the reference Claude apps gateway, captured
/// live from `claude gateway` 2.1.214 on 2026-07-18. An upstream catalog change
/// should be reflected by updating this one table.
const BUILTIN_MODEL_IDS: &[&str] = &[
    "claude-opus-4-6",
    "claude-sonnet-4-5-20250929",
    "claude-haiku-4-5-20251001",
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-1-20250805",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
];

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

pub async fn get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Snapshot the live config so this response reflects the latest reload.
    let state = state.refreshed();
    let static_client = state
        .inbound_auth
        .as_ref()
        .and_then(|auth| auth.authenticate_client(&headers));
    let gateway_identity = state
        .gateway_auth
        .as_ref()
        .and_then(|auth| auth.authenticate_bearer(&headers));
    if (state.inbound_auth.is_some() || state.gateway_auth.is_some())
        && static_client.is_none()
        && gateway_identity.is_none()
    {
        tracing::warn!(
            "inbound auth failed for GET /v1/models: missing or invalid client credential"
        );
        let message = match (&state.inbound_auth, &state.gateway_auth) {
            (Some(auth), Some(_)) => format!(
                "missing or invalid credential: this gateway requires a client token (via {}, x-api-key, or Authorization: Bearer) or gateway login for model discovery",
                auth.header()
            ),
            (Some(auth), None) => format!(
                "missing or invalid credential: this gateway requires a client token (via {}, x-api-key, or Authorization: Bearer) for model discovery; ask the operator for one",
                auth.header()
            ),
            (None, Some(_)) => {
                "missing or invalid credential: sign in to this gateway for model discovery"
                    .to_string()
            }
            (None, None) => unreachable!("authentication gate requires configured auth"),
        };
        return ShuntError::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
            .into_response();
    }
    if let Some(client) = static_client {
        tracing::info!(client = %client, "inbound client authenticated for GET /v1/models");
    } else if let Some(identity) = gateway_identity {
        tracing::info!(client = %identity.email, "gateway user authenticated for GET /v1/models");
    }
    let mut data: Vec<ModelEntry> = state
        .config
        .models
        .iter()
        .map(|model| ModelEntry {
            id: model.id.clone(),
            display_name: model.display_name.clone(),
        })
        .collect();
    if state.config.auto_include_builtin_models {
        for &id in BUILTIN_MODEL_IDS {
            if data.iter().all(|model| model.id != id) {
                data.push(ModelEntry {
                    id: id.to_string(),
                    // The reference repeats the id as display_name. Claude Code
                    // falls back to the id, so omit it for an equivalent smaller body.
                    display_name: None,
                });
            }
        }
    }
    tracing::info!(models = data.len(), "served GET /v1/models discovery");
    Json(ModelsResponse { data }).into_response()
}

#[cfg(test)]
mod tests {
    use axum::{extract::State, http::HeaderMap};
    use serde_json::json;

    use crate::{
        config::ModelConfig,
        server::{self, AppState},
    };

    use super::get;

    #[tokio::test]
    async fn returns_configured_models_with_optional_display_name() {
        let config = crate::config::Config {
            auto_include_builtin_models: false,
            models: vec![
                ModelConfig {
                    id: "claude-opus-via-codex".to_string(),
                    display_name: Some("Opus (via Codex)".to_string()),
                },
                ModelConfig {
                    id: "anthropic-sonnet-via-codex".to_string(),
                    display_name: None,
                },
            ],
            ..crate::config::Config::default()
        };
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test".parse().unwrap());

        let response = get(State(state), headers).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            body,
            json!({
                "data": [
                    {"id": "claude-opus-via-codex", "display_name": "Opus (via Codex)"},
                    {"id": "anthropic-sonnet-via-codex"}
                ]
            })
        );
    }

    #[tokio::test]
    async fn returns_empty_data_when_models_are_unconfigured() {
        let config = crate::config::Config {
            auto_include_builtin_models: false,
            ..crate::config::Config::default()
        };
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        let response = get(State(state), HeaderMap::new()).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(body, json!({"data": []}));
    }

    #[tokio::test]
    async fn default_returns_builtin_models_in_reference_order() {
        let state =
            AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();

        let response = get(State(state), HeaderMap::new()).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            body,
            json!({
                "data": [
                    {"id": "claude-opus-4-6"},
                    {"id": "claude-sonnet-4-5-20250929"},
                    {"id": "claude-haiku-4-5-20251001"},
                    {"id": "claude-fable-5"},
                    {"id": "claude-opus-4-8"},
                    {"id": "claude-opus-4-7"},
                    {"id": "claude-opus-4-1-20250805"},
                    {"id": "claude-sonnet-5"},
                    {"id": "claude-sonnet-4-6"}
                ]
            })
        );
    }

    #[tokio::test]
    async fn curated_models_precede_and_override_matching_builtins() {
        let config = crate::config::Config {
            models: vec![
                ModelConfig {
                    id: "claude-opus-4-8".to_string(),
                    display_name: Some("Opus Curated".to_string()),
                },
                ModelConfig {
                    id: "claude-custom-model".to_string(),
                    display_name: None,
                },
            ],
            ..crate::config::Config::default()
        };
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        let response = get(State(state), HeaderMap::new()).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            body,
            json!({
                "data": [
                    {"id": "claude-opus-4-8", "display_name": "Opus Curated"},
                    {"id": "claude-custom-model"},
                    {"id": "claude-opus-4-6"},
                    {"id": "claude-sonnet-4-5-20250929"},
                    {"id": "claude-haiku-4-5-20251001"},
                    {"id": "claude-fable-5"},
                    {"id": "claude-opus-4-7"},
                    {"id": "claude-opus-4-1-20250805"},
                    {"id": "claude-sonnet-5"},
                    {"id": "claude-sonnet-4-6"}
                ]
            })
        );
    }

    #[test]
    fn router_includes_get_models_route() {
        let (_router, _shared, _state) =
            server::build_router(crate::config::Config::default()).unwrap();
    }
}
