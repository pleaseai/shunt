use std::net::{IpAddr, SocketAddr};

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{ConnectInfo, Request, State},
    http::{header::CONTENT_LENGTH, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{
    concurrency::is_codex_path,
    config::{AccessControlConfig, LimitsConfig},
    error::{into_openai_error_shape, ShuntError, UpstreamError},
    gateway::device::client_ip,
};

#[derive(Clone, Debug)]
pub(crate) struct HttpTuningLayer {
    access_control: AccessControlConfig,
    limits: LimitsConfig,
}

impl HttpTuningLayer {
    pub(crate) fn new(access_control: AccessControlConfig, limits: LimitsConfig) -> Self {
        Self {
            access_control,
            limits,
        }
    }
}

pub(crate) async fn enforce_http_tuning(
    State(tuning): State<HttpTuningLayer>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let codex_shape = is_codex_path(path);
    let allow_exempt = matches!(path, "/" | "/health");
    if tuning.access_control.enabled() {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| *address);
        let client = client_ip(
            request.headers(),
            peer,
            tuning.access_control.trust_forwarded_for,
        );
        let client = client
            .parse::<IpAddr>()
            .or_else(|_| client.parse::<SocketAddr>().map(|address| address.ip()))
            .ok();
        if !tuning.access_control.allows(client, allow_exempt) {
            return owned_error(
                StatusCode::FORBIDDEN,
                "permission_error",
                "client address is not allowed",
                codex_shape,
            )
            .await;
        }
    }
    if let Some(limit) = tuning.limits.max_url_length {
        let url_length = request
            .uri()
            .path_and_query()
            .map_or(0, |path_and_query| path_and_query.as_str().len());
        if url_length > limit {
            return owned_error(
                StatusCode::URI_TOO_LONG,
                "request_too_large",
                "request URL exceeds the configured limit",
                codex_shape,
            )
            .await;
        }
    }
    if let Some(limit) = tuning.limits.max_request_header_bytes {
        let size = request
            .headers()
            .iter()
            .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
            .sum::<usize>();
        if size > limit {
            return owned_error(
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                "request_too_large",
                "request headers exceed the configured limit",
                codex_shape,
            )
            .await;
        }
    }
    next.run(request).await
}

pub(crate) fn content_length_exceeds(request: &axum::http::HeaderMap, limit: usize) -> bool {
    request
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit as u64)
}

pub(crate) async fn read_body(
    body: Body,
    limit: usize,
    codex_shape: bool,
) -> Result<Bytes, Box<Response>> {
    // The error is boxed to keep this `Result` small: an `axum` `Response` is far
    // larger than the `Bytes` success value, and every caller only moves the error
    // into its own boxed error payload.
    match to_bytes(body, limit).await {
        Ok(body) => Ok(body),
        Err(error) if body_limit_exceeded(&error) => {
            Err(Box::new(request_too_large(codex_shape).await))
        }
        Err(error) => Err(Box::new(body_read_error(error, codex_shape).await)),
    }
}

fn body_limit_exceeded(error: &axum::Error) -> bool {
    std::error::Error::source(error)
        .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
}

async fn body_read_error(error: axum::Error, codex_shape: bool) -> Response {
    let response = UpstreamError::from_message(error.to_string()).into_response();
    if codex_shape {
        into_openai_error_shape(response).await
    } else {
        response
    }
}

pub(crate) async fn request_too_large(codex_shape: bool) -> Response {
    owned_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "request_too_large",
        "request body exceeds the configured limit",
        codex_shape,
    )
    .await
}

async fn owned_error(
    status: StatusCode,
    error_type: &'static str,
    message: &'static str,
    codex_shape: bool,
) -> Response {
    let response = ShuntError::new(status, error_type, message).into_response();
    if codex_shape {
        into_openai_error_shape(response).await
    } else {
        response
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        extract::Request,
        middleware,
        routing::{get, post},
        Router,
    };
    use futures_util::stream;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    fn access(allow: &[&str], deny: &[&str]) -> AccessControlConfig {
        let mut config = AccessControlConfig::default();
        config.allow_cidrs = allow.iter().map(|value| (*value).to_string()).collect();
        config.deny_cidrs = deny.iter().map(|value| (*value).to_string()).collect();
        config.validate().unwrap();
        config
    }

    fn app(access_control: AccessControlConfig, limits: LimitsConfig) -> Router {
        Router::new()
            .route("/", get(|| async { StatusCode::NO_CONTENT }))
            .route("/health", get(|| async { StatusCode::NO_CONTENT }))
            .route("/v1/messages", post(|| async { StatusCode::NO_CONTENT }))
            .route("/v1/responses", post(|| async { StatusCode::NO_CONTENT }))
            .layer(middleware::from_fn_with_state(
                HttpTuningLayer::new(access_control, limits),
                enforce_http_tuning,
            ))
    }

    fn request(path: &str, peer: Option<IpAddr>) -> Request {
        let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
        if let Some(peer) = peer {
            request
                .extensions_mut()
                .insert(ConnectInfo(SocketAddr::new(peer, 1)));
        }
        request
    }

    async fn body(response: Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn deny_wins_over_allow() {
        let response = app(
            access(&["10.0.0.0/8"], &["10.1.0.0/16"]),
            LimitsConfig::default(),
        )
        .oneshot(request("/v1/messages", Some("10.1.2.3".parse().unwrap())))
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn allow_list_is_default_deny_and_missing_peer_fails_closed() {
        let configured = access(&["10.0.0.0/8"], &[]);
        for peer in [Some("192.0.2.1".parse().unwrap()), None] {
            let response = app(configured.clone(), LimitsConfig::default())
                .oneshot(request("/v1/messages", peer))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test]
    async fn missing_or_malformed_address_fails_closed_when_policy_is_enabled() {
        let denied_only = access(&[], &["192.0.2.0/24"]);
        let missing = request("/v1/messages", None);
        let mut malformed = request("/v1/messages", None);
        malformed
            .headers_mut()
            .insert("x-forwarded-for", "not-an-ip".parse().unwrap());
        let mut denied_only = denied_only;
        denied_only.trust_forwarded_for = true;
        for request in [missing, malformed] {
            let response = app(denied_only.clone(), LimitsConfig::default())
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test]
    async fn access_policy_fails_closed_when_unvalidated() {
        let mut configured = AccessControlConfig::default();
        configured.allow_cidrs = vec!["10.0.0.0/8".to_string()];
        let response = app(configured, LimitsConfig::default())
            .oneshot(request("/v1/messages", Some("10.1.2.3".parse().unwrap())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn forwarded_socket_address_is_checked_by_ip() {
        let mut configured = access(&["198.51.100.0/24"], &[]);
        configured.trust_forwarded_for = true;
        let mut forwarded = Request::post("/v1/messages").body(Body::empty()).unwrap();
        forwarded
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(
                "203.0.113.1".parse().unwrap(),
                1,
            )));
        forwarded
            .headers_mut()
            .insert("x-forwarded-for", "198.51.100.4:43123".parse().unwrap());
        let response = app(configured, LimitsConfig::default())
            .oneshot(forwarded)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn health_is_exempt_from_allow_but_not_deny() {
        let response = app(access(&["10.0.0.0/8"], &[]), LimitsConfig::default())
            .oneshot(request("/health", Some("192.0.2.1".parse().unwrap())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = app(access(&[], &["192.0.2.0/24"]), LimitsConfig::default())
            .oneshot(request("/health", Some("192.0.2.1".parse().unwrap())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn access_errors_use_protocol_specific_shapes() {
        let configured = access(&[], &["192.0.2.0/24"]);
        let anthropic = app(configured.clone(), LimitsConfig::default())
            .oneshot(request("/v1/messages", Some("192.0.2.1".parse().unwrap())))
            .await
            .unwrap();
        let anthropic = body(anthropic).await;
        assert_eq!(anthropic["type"], "error");

        let codex = app(configured, LimitsConfig::default())
            .oneshot(request("/v1/responses", Some("192.0.2.1".parse().unwrap())))
            .await
            .unwrap();
        let codex = body(codex).await;
        assert!(codex.get("type").is_none());
        assert_eq!(codex["error"]["type"], "permission_error");
    }

    #[tokio::test]
    async fn chunked_body_without_content_length_hits_streaming_backstop() {
        let streamed_body = Body::from_stream(stream::iter([
            Ok::<_, std::convert::Infallible>(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def")),
        ]));
        let response = read_body(streamed_body, 4, false)
            .await
            .expect_err("six streamed bytes exceed the four-byte limit");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body(*response).await["error"]["type"], "request_too_large");
    }

    #[tokio::test]
    async fn body_errors_use_protocol_specific_shapes() {
        for (codex_shape, expected_type) in [(false, "api_error"), (true, "api_error")] {
            let streamed_body = Body::from_stream(stream::iter([Err::<Bytes, _>(
                std::io::Error::other("client body failed"),
            )]));
            let response = read_body(streamed_body, 1024, codex_shape)
                .await
                .expect_err("the body source error should be preserved");
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let response = body(*response).await;
            if codex_shape {
                assert!(response.get("type").is_none());
                assert_eq!(response["error"]["type"], expected_type);
            } else {
                assert_eq!(response["error"]["type"], expected_type);
            }
        }
    }

    #[tokio::test]
    async fn rejects_content_length_headers_and_urls_at_their_limits() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(CONTENT_LENGTH, "5".parse().unwrap());
        assert!(content_length_exceeds(&headers, 4));
        let response = request_too_large(false).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let response = app(
            AccessControlConfig::default(),
            LimitsConfig {
                max_request_header_bytes: Some(7),
                ..LimitsConfig::default()
            },
        )
        .oneshot(
            Request::post("/v1/messages")
                .header("x-a", "12345")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );

        let response = app(
            AccessControlConfig::default(),
            LimitsConfig {
                max_url_length: Some(8),
                ..LimitsConfig::default()
            },
        )
        .oneshot(request("/health?long=yes", None))
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::URI_TOO_LONG);
    }
}
