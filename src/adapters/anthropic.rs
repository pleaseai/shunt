use axum::{
    body::Body,
    http::{HeaderMap, Response, StatusCode, Uri},
    response::IntoResponse,
};

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    error::UpstreamError,
    headers,
    routing::Route,
    server::AppState,
};

pub struct AnthropicAdapter;

impl Adapter for AnthropicAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        _route: Route,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        body: Vec<u8>,
    ) -> AdapterFuture<'a> {
        Box::pin(async move { forward(state, uri, headers, body).await })
    }
}

async fn forward(
    state: AppState,
    uri: &Uri,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let upstream = state
        .http_client
        .post(upstream_url(&state, uri))
        .headers(headers::filtered(headers))
        .body(body)
        .send()
        .await
        .map_err(upstream_error)?;
    let status = upstream.status();
    let response_headers = headers::filtered(upstream.headers());
    let stream = upstream.bytes_stream();

    let mut builder = Response::builder().status(status);
    for (name, value) in response_headers {
        if let Some(name) = name {
            builder = builder.header(name, value);
        }
    }

    let response = builder
        .body(Body::from_stream(stream))
        .expect("response builder uses valid upstream status and headers")
        .into_response();
    Ok((status, response))
}

fn upstream_url(state: &AppState, uri: &Uri) -> String {
    let base = state
        .config
        .providers
        .anthropic
        .base_url
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
