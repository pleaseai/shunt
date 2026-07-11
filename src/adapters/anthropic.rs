use std::time::Duration;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Response, StatusCode, Uri},
    response::IntoResponse,
};

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{resolve_credential, Credential},
    config::ApiKeyHeader,
    error::UpstreamError,
    headers, keepalive,
    routing::Route,
    server::AppState,
};

pub struct AnthropicAdapter;

impl Adapter for AnthropicAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        body: Vec<u8>,
    ) -> AdapterFuture<'a> {
        Box::pin(async move { forward(state, route, uri, headers, body).await })
    }
}

async fn forward(
    state: AppState,
    route: Route,
    uri: &Uri,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    let request_headers = outbound_headers(headers, &credential);
    let body = normalize_upstream_model(body, &route.upstream_model);
    let upstream = state
        .http_client
        .post(upstream_url(&state, &route, uri))
        .headers(request_headers)
        .body(body)
        .send()
        .await
        .map_err(upstream_error)?;
    let status = upstream.status();
    let response_headers = headers::filtered(upstream.headers());
    let is_sse = upstream
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("text/event-stream"))
        .unwrap_or(false);
    let stream = upstream.bytes_stream();

    let mut builder = Response::builder().status(status);
    for (name, value) in response_headers {
        if let Some(name) = name {
            builder = builder.header(name, value);
        }
    }

    // Keepalive pings apply only to SSE relays; JSON bodies pass untouched.
    let body = if is_sse {
        Body::from_stream(keepalive::with_pings(
            stream,
            Duration::from_secs(state.config.server.sse_keepalive_seconds),
        ))
    } else {
        Body::from_stream(stream)
    };
    let response = builder
        .body(body)
        .expect("response builder uses valid upstream status and headers")
        .into_response();
    Ok((status, response))
}

/// Rewrite the outbound request body's `model` to the routed `upstream_model`
/// when they differ. The passthrough adapter forwards the client body verbatim,
/// so without this two things leak to the provider: a `[1m]` context-window hint
/// (which `routing::strip_context_window_hint` removes from the routing key but
/// not from the body — and api.anthropic.com does not recognize a `[1m]`-suffixed
/// model id), and an explicit `[[routes]]` `upstream_model` remap (otherwise
/// ignored for an Anthropic-provider route). The common case — body model already
/// equal to `upstream_model` — re-serializes nothing and forwards the original
/// bytes untouched, preserving byte-for-byte passthrough.
fn normalize_upstream_model(body: Vec<u8>, upstream_model: &str) -> Vec<u8> {
    #[derive(serde::Deserialize)]
    struct ModelView {
        model: String,
    }

    // Cheap guard: peek only the `model` field. A body that isn't JSON, has no
    // `model`, or whose model already matches is forwarded unchanged.
    match serde_json::from_slice::<ModelView>(&body) {
        Ok(view) if view.model != upstream_model => {
            let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&body) else {
                return body;
            };
            let Some(object) = value.as_object_mut() else {
                return body;
            };
            object.insert(
                "model".to_string(),
                serde_json::Value::String(upstream_model.to_string()),
            );
            serde_json::to_vec(&value).unwrap_or(body)
        }
        _ => body,
    }
}

/// Build the headers sent upstream. For a passthrough provider (api.anthropic.com)
/// the client's own credential is forwarded unchanged. For an api-key provider
/// (Kimi, DeepSeek, Z.ai, OpenRouter, Vercel, …) the client's auth headers are
/// stripped and replaced with the provider's key in its configured header.
fn outbound_headers(headers: &HeaderMap, credential: &Credential) -> HeaderMap {
    let mut out = headers::filtered(headers);
    if let Credential::ApiKey { value, header } = credential {
        out.remove("authorization");
        out.remove("x-api-key");
        match header {
            ApiKeyHeader::Bearer => {
                if let Ok(value) = HeaderValue::from_str(&format!("Bearer {value}")) {
                    out.insert("authorization", value);
                }
            }
            ApiKeyHeader::XApiKey => {
                if let Ok(value) = HeaderValue::from_str(value) {
                    out.insert("x-api-key", value);
                }
            }
        }
    }
    out
}

fn upstream_url(state: &AppState, route: &Route, uri: &Uri) -> String {
    let base = state
        .config
        .provider(&route.provider)
        .map(|provider| provider.base_url.as_str())
        .unwrap_or("https://api.anthropic.com")
        .trim_end_matches('/');
    let path_and_query = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(uri.path());
    format!("{base}{path_and_query}")
}

fn upstream_error(error: reqwest::Error) -> AdapterError {
    let message = error.to_string();
    AdapterError {
        message,
        response: Box::new(UpstreamError::from_reqwest(error).into_response()),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;

    use crate::config::ApiKeyHeader;

    use super::{normalize_upstream_model, outbound_headers, Credential};

    fn client_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer client-token".parse().unwrap());
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers
    }

    #[test]
    fn passthrough_forwards_client_credential_unchanged() {
        let out = outbound_headers(&client_headers(), &Credential::Passthrough);
        assert_eq!(out.get("authorization").unwrap(), "Bearer client-token");
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn api_key_bearer_replaces_client_credential() {
        let out = outbound_headers(
            &client_headers(),
            &Credential::ApiKey {
                value: "provider-key".to_string(),
                header: ApiKeyHeader::Bearer,
            },
        );
        assert_eq!(out.get("authorization").unwrap(), "Bearer provider-key");
        assert!(out.get("x-api-key").is_none());
        // Non-auth client headers still pass through.
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn api_key_x_api_key_replaces_client_credential() {
        let out = outbound_headers(
            &client_headers(),
            &Credential::ApiKey {
                value: "provider-key".to_string(),
                header: ApiKeyHeader::XApiKey,
            },
        );
        assert_eq!(out.get("x-api-key").unwrap(), "provider-key");
        assert!(out.get("authorization").is_none());
    }

    #[test]
    fn normalize_rewrites_model_when_upstream_differs() {
        // A `[1m]` context-window hint must not reach the provider verbatim.
        let body = br#"{"model":"claude-sonnet-4-6[1m]","max_tokens":1}"#.to_vec();
        let out = normalize_upstream_model(body, "claude-sonnet-4-6");
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["model"], "claude-sonnet-4-6");
        // The rest of the body survives the rewrite.
        assert_eq!(value["max_tokens"], 1);
    }

    #[test]
    fn normalize_leaves_body_untouched_when_model_matches() {
        // Common case: byte-for-byte passthrough, no re-serialization.
        let body = br#"{"model":"claude-sonnet-4-6","max_tokens":1}"#.to_vec();
        let original = body.clone();
        let out = normalize_upstream_model(body, "claude-sonnet-4-6");
        assert_eq!(out, original);
    }

    #[test]
    fn normalize_leaves_non_json_body_untouched() {
        let body = b"not json".to_vec();
        let original = body.clone();
        let out = normalize_upstream_model(body, "claude-sonnet-4-6");
        assert_eq!(out, original);
    }
}
