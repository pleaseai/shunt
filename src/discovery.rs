use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::{error::ShuntError, server::AppState};

mod upstream;

/// Builtin catalog captured live from `GET https://api.anthropic.com/v1/models`
/// on 2026-07-28, in the API's own order (`created_at` descending, newest
/// first).
///
/// The upstream list is **credential-scoped**: the same endpoint returned 11
/// entries for an `x-api-key` caller, 10 for a Claude subscription OAuth bearer
/// (no `claude-opus-4-1-20250805`), and the reference `claude gateway` 2.1.220
/// serves a third 10-entry variant (no `claude-opus-4-5-20251101`). This table
/// is the **superset** of all three, so no caller loses a model it can actually
/// reach. The cost is that a caller may see an id its own credential is not
/// entitled to; consistent with the existing stance, that stays a runtime error
/// rather than an upstream entitlement probe at discovery time.
///
/// An upstream catalog change should be reflected by updating this one table.
struct BuiltinModel {
    id: &'static str,
    display_name: &'static str,
    created_at: &'static str,
    max_input_tokens: u64,
    max_tokens: u64,
}

const BUILTIN_MODELS: &[BuiltinModel] = &[
    BuiltinModel {
        id: "claude-opus-5",
        display_name: "Claude Opus 5",
        created_at: "2026-07-24T00:00:00Z",
        max_input_tokens: 1_000_000,
        max_tokens: 128_000,
    },
    BuiltinModel {
        id: "claude-sonnet-5",
        display_name: "Claude Sonnet 5",
        created_at: "2026-06-29T00:00:00Z",
        max_input_tokens: 1_000_000,
        max_tokens: 128_000,
    },
    BuiltinModel {
        id: "claude-fable-5",
        display_name: "Claude Fable 5",
        created_at: "2026-06-07T00:00:00Z",
        max_input_tokens: 1_000_000,
        max_tokens: 128_000,
    },
    BuiltinModel {
        id: "claude-opus-4-8",
        display_name: "Claude Opus 4.8",
        created_at: "2026-05-28T00:00:00Z",
        max_input_tokens: 1_000_000,
        max_tokens: 128_000,
    },
    BuiltinModel {
        id: "claude-opus-4-7",
        display_name: "Claude Opus 4.7",
        created_at: "2026-04-14T00:00:00Z",
        max_input_tokens: 1_000_000,
        max_tokens: 128_000,
    },
    BuiltinModel {
        id: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        created_at: "2026-02-17T00:00:00Z",
        max_input_tokens: 1_000_000,
        max_tokens: 128_000,
    },
    BuiltinModel {
        id: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        created_at: "2026-02-04T00:00:00Z",
        max_input_tokens: 1_000_000,
        max_tokens: 128_000,
    },
    BuiltinModel {
        id: "claude-opus-4-5-20251101",
        display_name: "Claude Opus 4.5",
        created_at: "2025-11-24T00:00:00Z",
        max_input_tokens: 200_000,
        max_tokens: 64_000,
    },
    BuiltinModel {
        id: "claude-haiku-4-5-20251001",
        display_name: "Claude Haiku 4.5",
        created_at: "2025-10-15T00:00:00Z",
        max_input_tokens: 200_000,
        max_tokens: 64_000,
    },
    BuiltinModel {
        id: "claude-sonnet-4-5-20250929",
        display_name: "Claude Sonnet 4.5",
        created_at: "2025-09-29T00:00:00Z",
        max_input_tokens: 1_000_000,
        max_tokens: 64_000,
    },
    BuiltinModel {
        id: "claude-opus-4-1-20250805",
        display_name: "Claude Opus 4.1",
        created_at: "2025-08-05T00:00:00Z",
        max_input_tokens: 200_000,
        max_tokens: 32_000,
    },
];

/// Anthropic list envelope. shunt never paginates, so `has_more` is constant,
/// but `first_id`/`last_id` mirror the real API and carry the first and last
/// entry ids (null only when `data` is empty).
#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub data: Vec<ModelEntry>,
    pub has_more: bool,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
}

/// Field order mirrors the upstream API. Everything past `id` is optional so a
/// curated `[[models]]` entry, which carries no upstream metadata, serializes to
/// the same narrow shape it always did.
#[derive(Debug, Serialize)]
pub struct ModelEntry {
    #[serde(rename = "type")]
    pub entry_type: &'static str,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Only ever populated from a live upstream list; relayed verbatim and never
    /// synthesized, so the builtin snapshot omits it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<serde_json::Value>,
}

impl ModelEntry {
    pub fn new(id: String, display_name: Option<String>) -> Self {
        Self {
            entry_type: "model",
            id,
            display_name,
            created_at: None,
            max_input_tokens: None,
            max_tokens: None,
            capabilities: None,
        }
    }

    fn builtin(model: &'static BuiltinModel) -> Self {
        Self {
            entry_type: "model",
            id: model.id.to_string(),
            display_name: Some(model.display_name.to_string()),
            created_at: Some(model.created_at.to_string()),
            max_input_tokens: Some(model.max_input_tokens),
            max_tokens: Some(model.max_tokens),
            capabilities: None,
        }
    }
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
    } else if let Some(identity) = gateway_identity.as_ref() {
        tracing::info!(client = %identity.email, "gateway user authenticated for GET /v1/models");
    }
    let mut data: Vec<ModelEntry> = state
        .config
        .models
        .iter()
        .map(|model| ModelEntry::new(model.id.clone(), model.display_name.clone()))
        .collect();
    if state.config.auto_include_builtin_models {
        // Ask the upstream for this caller's own catalog first; the builtin
        // table is the offline snapshot used when there is no Anthropic-kind
        // upstream, no credential to ask with, or the call fails.
        match upstream::fetch(
            &state,
            &headers,
            upstream::InboundCredentialContext {
                static_auth: state.inbound_auth.as_deref(),
                gateway_bearer_authenticated: gateway_identity.is_some(),
            },
        )
        .await
        {
            Some(models) => {
                for model in models {
                    if data.iter().all(|entry| entry.id != model.id) {
                        data.push(model);
                    }
                }
            }
            None => {
                for model in BUILTIN_MODELS {
                    if data.iter().all(|entry| entry.id != model.id) {
                        data.push(ModelEntry::builtin(model));
                    }
                }
            }
        }
    }
    tracing::info!(models = data.len(), "served GET /v1/models discovery");
    // Cursor fields mirror the upstream API, which populates them from the page
    // even when it does not paginate.
    let first_id = data.first().map(|entry| entry.id.clone());
    let last_id = data.last().map(|entry| entry.id.clone());
    Json(ModelsResponse {
        data,
        has_more: false,
        first_id,
        last_id,
    })
    .into_response()
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
                    upstream_model: Some(std::collections::BTreeMap::from([(
                        "codex".to_string(),
                        "gpt-5.2".to_string(),
                    )])),
                },
                ModelConfig {
                    id: "anthropic-sonnet-via-codex".to_string(),
                    display_name: None,
                    upstream_model: None,
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
                    {"type": "model", "id": "claude-opus-via-codex", "display_name": "Opus (via Codex)"},
                    {"type": "model", "id": "anthropic-sonnet-via-codex"}
                ],
                "has_more": false,
                "first_id": "claude-opus-via-codex",
                "last_id": "anthropic-sonnet-via-codex"
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

        assert_eq!(
            body,
            json!({"data": [], "has_more": false, "first_id": null, "last_id": null})
        );
    }

    #[tokio::test]
    async fn default_returns_builtin_models_in_api_order() {
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
                    {"type": "model", "id": "claude-opus-5", "display_name": "Claude Opus 5", "created_at": "2026-07-24T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-sonnet-5", "display_name": "Claude Sonnet 5", "created_at": "2026-06-29T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-fable-5", "display_name": "Claude Fable 5", "created_at": "2026-06-07T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-opus-4-8", "display_name": "Claude Opus 4.8", "created_at": "2026-05-28T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-opus-4-7", "display_name": "Claude Opus 4.7", "created_at": "2026-04-14T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6", "created_at": "2026-02-17T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-opus-4-6", "display_name": "Claude Opus 4.6", "created_at": "2026-02-04T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-opus-4-5-20251101", "display_name": "Claude Opus 4.5", "created_at": "2025-11-24T00:00:00Z", "max_input_tokens": 200000, "max_tokens": 64000},
                    {"type": "model", "id": "claude-haiku-4-5-20251001", "display_name": "Claude Haiku 4.5", "created_at": "2025-10-15T00:00:00Z", "max_input_tokens": 200000, "max_tokens": 64000},
                    {"type": "model", "id": "claude-sonnet-4-5-20250929", "display_name": "Claude Sonnet 4.5", "created_at": "2025-09-29T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 64000},
                    {"type": "model", "id": "claude-opus-4-1-20250805", "display_name": "Claude Opus 4.1", "created_at": "2025-08-05T00:00:00Z", "max_input_tokens": 200000, "max_tokens": 32000}
                ],
                "has_more": false,
                "first_id": "claude-opus-5",
                "last_id": "claude-opus-4-1-20250805"
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
                    upstream_model: None,
                },
                ModelConfig {
                    id: "claude-custom-model".to_string(),
                    display_name: None,
                    upstream_model: None,
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
                    {"type": "model", "id": "claude-opus-4-8", "display_name": "Opus Curated"},
                    {"type": "model", "id": "claude-custom-model"},
                    {"type": "model", "id": "claude-opus-5", "display_name": "Claude Opus 5", "created_at": "2026-07-24T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-sonnet-5", "display_name": "Claude Sonnet 5", "created_at": "2026-06-29T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-fable-5", "display_name": "Claude Fable 5", "created_at": "2026-06-07T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-opus-4-7", "display_name": "Claude Opus 4.7", "created_at": "2026-04-14T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6", "created_at": "2026-02-17T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-opus-4-6", "display_name": "Claude Opus 4.6", "created_at": "2026-02-04T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 128000},
                    {"type": "model", "id": "claude-opus-4-5-20251101", "display_name": "Claude Opus 4.5", "created_at": "2025-11-24T00:00:00Z", "max_input_tokens": 200000, "max_tokens": 64000},
                    {"type": "model", "id": "claude-haiku-4-5-20251001", "display_name": "Claude Haiku 4.5", "created_at": "2025-10-15T00:00:00Z", "max_input_tokens": 200000, "max_tokens": 64000},
                    {"type": "model", "id": "claude-sonnet-4-5-20250929", "display_name": "Claude Sonnet 4.5", "created_at": "2025-09-29T00:00:00Z", "max_input_tokens": 1000000, "max_tokens": 64000},
                    {"type": "model", "id": "claude-opus-4-1-20250805", "display_name": "Claude Opus 4.1", "created_at": "2025-08-05T00:00:00Z", "max_input_tokens": 200000, "max_tokens": 32000}
                ],
                "has_more": false,
                "first_id": "claude-opus-4-8",
                "last_id": "claude-opus-4-1-20250805"
            })
        );
    }

    #[tokio::test]
    async fn live_upstream_list_supersedes_the_builtin_snapshot() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"type": "model", "id": "claude-opus-5", "display_name": "Claude Opus 5"},
                    {"type": "model", "id": "claude-only-upstream-knows"}
                ],
                "has_more": false,
                "first_id": "claude-opus-5",
                "last_id": "claude-only-upstream-knows"
            })))
            .mount(&server)
            .await;

        let mut config = crate::config::Config {
            models: vec![ModelConfig {
                id: "claude-opus-5".to_string(),
                display_name: Some("Opus Curated".to_string()),
                upstream_model: None,
            }],
            ..crate::config::Config::default()
        };
        config.providers.get_mut("anthropic").unwrap().base_url = server.uri();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "caller-key".parse().unwrap());

        let response = get(State(state), headers).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Curated entry keeps its position and label, the upstream id the
        // builtin table has never heard of is present, and no builtin-only id
        // (e.g. claude-opus-4-1-20250805) was appended on top of a live answer.
        assert_eq!(
            body,
            json!({
                "data": [
                    {"type": "model", "id": "claude-opus-5", "display_name": "Opus Curated"},
                    {"type": "model", "id": "claude-only-upstream-knows"}
                ],
                "has_more": false,
                "first_id": "claude-opus-5",
                "last_id": "claude-only-upstream-knows"
            })
        );
    }

    #[test]
    fn router_includes_get_models_route() {
        let (_router, _shared, _state) =
            server::build_router(crate::config::Config::default()).unwrap();
    }
}
