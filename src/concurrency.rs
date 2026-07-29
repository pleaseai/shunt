use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    body::{Body, Bytes, HttpBody},
    extract::{Request, State},
    http::{header::RETRY_AFTER, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{into_openai_error_shape, ShuntError};

const OVERLOADED_MESSAGE: &str = "too many requests are already in flight";
const CODEX_PATHS: [&str; 5] = [
    "/backend-api/codex/responses",
    "/responses",
    "/v1/responses",
    "/backend-api/codex/analytics-events/events",
    "/codex/analytics-events/events",
];

/// Process-lifetime admission gate shared by every limited route.
#[derive(Clone, Debug)]
pub(crate) struct ConcurrencyLimit {
    permits: Arc<Semaphore>,
}

impl ConcurrencyLimit {
    pub(crate) fn new(max_concurrent_requests: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent_requests)),
        }
    }
}

/// Shed an over-limit request immediately, or hold its permit until the
/// response body reaches end-of-stream or is dropped by a disconnected client.
pub(crate) async fn limit_requests(
    State(limit): State<ConcurrencyLimit>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let permit = match limit.permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            // `debug!` because a saturated gateway would emit this per request;
            // the counter is what makes saturation visible at the default
            // `shunt=info` filter, since a shed request never reaches a handler
            // and so never lands in `record_proxied_request` or a request span.
            tracing::debug!(path, "shedding request at inbound concurrency limit");
            crate::metrics::record_request_shed();
            return overloaded_response(is_codex_path(path)).await;
        }
    };

    next.run(request)
        .await
        .map(|body| Body::new(PermitBody::new(body, permit)))
}

fn is_codex_path(path: &str) -> bool {
    CODEX_PATHS.contains(&path)
}

async fn overloaded_response(codex_shape: bool) -> Response {
    let response = ShuntError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "overloaded_error",
        OVERLOADED_MESSAGE,
    )
    .into_response();
    let mut response = if codex_shape {
        into_openai_error_shape(response).await
    } else {
        response
    };
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

/// Delegates every frame, including trailers, while owning the request permit.
#[derive(Debug)]
struct PermitBody {
    inner: Body,
    permit: Option<OwnedSemaphorePermit>,
}

impl PermitBody {
    fn new(inner: Body, permit: OwnedSemaphorePermit) -> Self {
        // Once `is_end_stream()` is true, `poll_frame` is contractually going to
        // yield `None`, so there is nothing left to wait for. Holding the permit
        // would keep a bodyless or already-complete response (a 204, an empty
        // error envelope) occupying a slot until whoever owns the wrapper
        // happens to drop it. Release it now instead — this is a semantic fast
        // path, not a micro-optimization.
        let permit = (!inner.is_end_stream()).then_some(permit);
        Self { inner, permit }
    }
}

impl HttpBody for PermitBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let frame = Pin::new(&mut self.inner).poll_frame(cx);
        // Releasing here rather than relying on `Drop` is load-bearing: a fully
        // consumed body can stay alive well after end-of-stream (hyper may hold
        // it while finishing the connection), and capacity should return to the
        // pool when the response actually finishes, not whenever this wrapper is
        // eventually dropped. `Drop` still covers the paths that never reach
        // EOS — client disconnect, or a body abandoned mid-stream.
        if matches!(frame, Poll::Ready(None)) {
            self.permit.take();
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        future::poll_fn,
        pin::Pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use axum::{
        body::{to_bytes, Body, HttpBody},
        http::{header::RETRY_AFTER, Request, StatusCode},
        middleware,
        response::Response,
        routing::post,
        Router,
    };
    use futures_util::stream;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::{limit_requests, ConcurrencyLimit};

    fn limited_router(path: &'static str, limit: usize) -> Router {
        Router::new()
            .route(path, post(|| async { StatusCode::NO_CONTENT }))
            .layer(middleware::from_fn_with_state(
                ConcurrencyLimit::new(limit),
                limit_requests,
            ))
    }

    async fn json_body(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn over_limit_uses_anthropic_shape_and_retry_after() {
        let response = limited_router("/v1/messages", 0)
            .oneshot(Request::post("/v1/messages").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "1");
        let body = json_body(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "overloaded_error");
    }

    #[tokio::test]
    async fn over_limit_codex_path_uses_openai_shape() {
        let response = limited_router("/v1/responses", 0)
            .oneshot(Request::post("/v1/responses").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "1");
        let body = json_body(response).await;
        assert!(body.get("type").is_none());
        assert_eq!(body["error"]["type"], "overloaded_error");
        assert!(body["error"]["code"].is_null());
    }

    #[tokio::test]
    async fn permit_is_released_when_stream_reaches_end_without_response_drop() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/stream",
                post(move || {
                    let call = calls.fetch_add(1, Ordering::Relaxed);
                    async move {
                        if call == 0 {
                            Body::from_stream(stream::iter([Ok::<_, Infallible>("done")]))
                        } else {
                            Body::empty()
                        }
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(
                ConcurrencyLimit::new(1),
                limit_requests,
            ));

        let first = app
            .clone()
            .oneshot(Request::post("/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // Drive the body to end-of-stream while keeping it alive. Consuming it
        // (e.g. `to_bytes`, which takes the body by value) would drop the
        // `PermitBody` and release the permit through `Drop`, so only holding
        // it across the assertion below proves the `poll_frame` EOS branch is
        // what released it.
        let (_parts, mut body) = first.into_parts();
        while poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .is_some()
        {}

        let second = app
            .oneshot(Request::post("/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        drop(body);
    }

    #[tokio::test]
    async fn permit_lives_until_streaming_body_is_dropped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app =
            Router::new()
                .route(
                    "/stream",
                    post(move || {
                        let call = calls.fetch_add(1, Ordering::Relaxed);
                        async move {
                            if call == 0 {
                                Body::from_stream(stream::pending::<
                                    Result<&'static [u8], Infallible>,
                                >())
                            } else {
                                Body::empty()
                            }
                        }
                    }),
                )
                .layer(middleware::from_fn_with_state(
                    ConcurrencyLimit::new(1),
                    limit_requests,
                ));

        let first = app
            .clone()
            .oneshot(Request::post("/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app
            .clone()
            .oneshot(Request::post("/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(first);
        let third = app
            .oneshot(Request::post("/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(third.status(), StatusCode::OK);
    }
}
