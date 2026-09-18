//! Separate local request failures from errors after HTTP dispatch.

use crate::{
    adapters::{AdapterError, AdapterFailure},
    auth::Credential,
    retry::RetryableError,
    routing::Route,
    server::AppState,
    upstream_timeout::{self, SendError},
};

use super::{body::PreparedBody, error::transport_error, request::request_builder};

#[derive(Debug)]
pub(super) enum RequestError {
    Local(String),
    Transport(reqwest::Error),
}

impl RequestError {
    pub(super) fn is_transport(&self) -> bool {
        matches!(self, Self::Transport(_))
    }

    pub(super) fn without_url(self) -> Self {
        match self {
            Self::Transport(error) => Self::Transport(error.without_url()),
            local => local,
        }
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(message) => formatter.write_str(message),
            Self::Transport(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}

impl RetryableError for RequestError {
    fn is_transient(&self) -> bool {
        match self {
            Self::Local(_) => false,
            Self::Transport(error) => error.is_transient(),
        }
    }
}

pub(super) fn send_error(error: SendError<RequestError>) -> AdapterError {
    error.into_adapter_error(|error| {
        let attempted = error.is_transport();
        let mut mapped = transport_error(error.to_string());
        if !attempted {
            mapped.failure = Some(AdapterFailure::NoUpstreamAttempt);
        }
        mapped
    })
}

/// Validate the complete request before the client attempts HTTP transport.
pub(super) async fn http_send(
    state: &AppState,
    route: &Route,
    credential: Credential,
    session_id: Option<&str>,
    body: PreparedBody,
) -> Result<reqwest::Response, SendError<RequestError>> {
    let request = body
        .attach(request_builder(state, route, credential, session_id))
        .build()
        .map_err(|error| {
            SendError::Transport(RequestError::Local(error.without_url().to_string()))
        })?;
    if !matches!(request.url().scheme(), "http" | "https") {
        return Err(SendError::Transport(RequestError::Local(
            "builder error".into(),
        )));
    }
    request
        .url()
        .as_str()
        .parse::<axum::http::Uri>()
        .map_err(|_| SendError::Transport(RequestError::Local("builder error".into())))?;

    // Redirects can return reqwest builder errors after an upstream response.
    // The production client permits HTTP and HTTPS.
    upstream_timeout::wait(state.config.server.timeouts.upstream_ttfb_ms, async {
        state
            .http_client
            .execute(request)
            .await
            .map_err(RequestError::Transport)
    })
    .await
}

#[cfg(test)]
mod tests;
