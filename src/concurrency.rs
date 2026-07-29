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
/// response body reaches a terminal frame (end-of-stream or an error) or the
/// body is dropped — a client disconnecting is the common cause of the latter,
/// but any owner dropping the body releases the permit just the same.
pub(crate) async fn limit_requests(
    State(limit): State<ConcurrencyLimit>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    // Move the `Arc` rather than cloning it: `try_acquire_owned` consumes an
    // `Arc<Semaphore>`, and axum's `State` extractor already handed us an owned
    // clone of the limiter, so cloning again would add a redundant refcount pair
    // per request on the hot path.
    let permit = match limit.permits.try_acquire_owned() {
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

/// Whether a gateway-owned error on this path must use the OpenAI Responses
/// envelope instead of the Anthropic one (AGENTS.md).
///
/// This middleware runs above routing, so it cannot ask the router which handler
/// would have matched and must classify by path. It reads the same constants the
/// router registers from, so a Codex route cannot be added without also getting
/// the correct error shape.
fn is_codex_path(path: &str) -> bool {
    crate::codex_endpoint::PATHS.contains(&path) || crate::codex_analytics::PATHS.contains(&path)
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
        // pool when the response finishes or terminates with an error, not when
        // this wrapper is eventually dropped. `Drop` still covers the paths that
        // never reach a terminal frame — client disconnect or a body abandoned
        // mid-stream.
        if matches!(frame, Poll::Ready(None | Some(Err(_)))) {
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
        task::Poll,
    };

    use axum::{
        body::{to_bytes, Body, Bytes, HttpBody},
        extract::Request,
        http::{header::RETRY_AFTER, HeaderMap, HeaderValue, StatusCode},
        middleware,
        response::Response,
        routing::post as post_route,
        Router,
    };
    use futures_util::{stream, StreamExt};
    use http_body::Frame;
    use http_body_util::StreamBody;
    use serde_json::Value;
    use tokio::sync::oneshot;
    use tower::ServiceExt;

    use super::{limit_requests, ConcurrencyLimit, PermitBody};

    fn limited_router_with_calls(
        path: &'static str,
        limit: usize,
        calls: Arc<AtomicUsize>,
    ) -> Router {
        Router::new()
            .route(
                path,
                post_route(move || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async { StatusCode::NO_CONTENT }
                }),
            )
            .layer(middleware::from_fn_with_state(
                ConcurrencyLimit::new(limit),
                limit_requests,
            ))
    }

    fn limited_router(path: &'static str, limit: usize) -> Router {
        limited_router_with_calls(path, limit, Arc::new(AtomicUsize::new(0)))
    }

    async fn json_body(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn send_post(app: &Router, path: &'static str) -> Response {
        app.clone()
            .oneshot(Request::post(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn over_limit_uses_anthropic_shape_and_retry_after() {
        let calls = Arc::new(AtomicUsize::new(0));
        let response = limited_router_with_calls("/v1/messages", 0, calls.clone())
            .oneshot(Request::post("/v1/messages").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "1");
        let body = json_body(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "overloaded_error");
        assert!(body["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn every_codex_path_uses_openai_shape_but_other_paths_do_not() {
        let calls = Arc::new(AtomicUsize::new(0));
        // Walk the same constants `server::build_router` registers from, so this
        // covers every real Codex route rather than a copy that could drift.
        for path in crate::codex_endpoint::PATHS
            .into_iter()
            .chain(crate::codex_analytics::PATHS)
        {
            let response = limited_router_with_calls(path, 0, calls.clone())
                .oneshot(Request::post(path).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(response.headers()[RETRY_AFTER], "1", "{path}");
            let body = json_body(response).await;
            assert!(body.get("type").is_none(), "{path}: {body}");
            assert_eq!(body["error"]["type"], "overloaded_error", "{path}");
            assert!(
                body["error"].get("code").is_some_and(Value::is_null),
                "{path}: {body}"
            );
            assert!(
                body["error"]["message"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty()),
                "{path}: {body}"
            );
            assert_eq!(calls.load(Ordering::Relaxed), 0, "{path}");
        }

        let path = "/v1/messages";
        let response = limited_router_with_calls(path, 0, calls.clone())
            .oneshot(Request::post(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "1");
        let body = json_body(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "overloaded_error");
        assert!(body["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn trailers_survive_the_permit_body_wrapper() {
        let limit = ConcurrencyLimit::new(1);
        let permit = limit.permits.clone().try_acquire_owned().unwrap();
        let mut trailers = HeaderMap::new();
        trailers.insert("x-stream-checksum", HeaderValue::from_static("verified"));
        let frames = stream::iter([
            Ok::<_, Infallible>(Frame::data(Bytes::from_static(b"chunk"))),
            Ok(Frame::trailers(trailers)),
        ]);
        let mut body = PermitBody::new(Body::new(StreamBody::new(frames)), permit);

        let data = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .expect("data frame")
            .unwrap()
            .into_data()
            .expect("first frame is data");
        assert_eq!(data, "chunk");
        let trailers = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .expect("trailer frame")
            .unwrap()
            .into_trailers()
            .expect("second frame is trailers");
        assert_eq!(trailers["x-stream-checksum"], "verified");
    }

    #[tokio::test]
    async fn permit_is_held_while_the_request_body_is_read() {
        let (body_tx, body_rx) = oneshot::channel::<Result<&'static str, Infallible>>();
        let (reading_tx, reading_rx) = oneshot::channel();
        let reading_tx = Arc::new(std::sync::Mutex::new(Some(reading_tx)));
        let app = Router::new()
            .route(
                "/upload",
                post_route(move |request: Request<Body>| {
                    let reading_tx = reading_tx.clone();
                    async move {
                        if let Some(reading_tx) = reading_tx.lock().unwrap().take() {
                            let _ = reading_tx.send(());
                        }
                        to_bytes(request.into_body(), usize::MAX).await.unwrap();
                        StatusCode::NO_CONTENT
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(
                ConcurrencyLimit::new(1),
                limit_requests,
            ));

        let first_app = app.clone();
        let first = tokio::spawn(async move {
            first_app
                .oneshot(
                    Request::post("/upload")
                        .body(Body::from_stream(stream::once(async move {
                            body_rx.await.expect("request body sender stays alive")
                        })))
                        .unwrap(),
                )
                .await
                .unwrap()
        });
        reading_rx.await.expect("handler starts reading body");

        let second = send_post(&app, "/upload").await;
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
        body_tx.send(Ok("complete")).unwrap();
        let first = first.await.unwrap();
        assert_eq!(first.status(), StatusCode::NO_CONTENT);
        let third = send_post(&app, "/upload").await;
        assert_eq!(third.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn limits_greater_than_one_admit_exactly_that_many_requests() {
        let app = Router::new()
            .route(
                "/stream",
                post_route(|| async {
                    Body::from_stream(stream::pending::<Result<&'static [u8], Infallible>>())
                }),
            )
            .layer(middleware::from_fn_with_state(
                ConcurrencyLimit::new(3),
                limit_requests,
            ));

        let first = send_post(&app, "/stream").await;
        let second = send_post(&app, "/stream").await;
        let third = send_post(&app, "/stream").await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(third.status(), StatusCode::OK);
        assert_eq!(
            send_post(&app, "/stream").await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        drop(second);
        let replacement = send_post(&app, "/stream").await;
        assert_eq!(replacement.status(), StatusCode::OK);
        assert_eq!(
            send_post(&app, "/stream").await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop((first, third, replacement));
    }

    #[tokio::test]
    async fn already_ended_body_releases_the_permit_immediately() {
        let app = limited_router("/empty", 1);

        let first = send_post(&app, "/empty").await;
        assert_eq!(first.status(), StatusCode::NO_CONTENT);
        let second = send_post(&app, "/empty").await;
        assert_eq!(second.status(), StatusCode::NO_CONTENT);
        drop((first, second));
    }

    #[tokio::test]
    async fn permit_is_released_when_stream_reaches_end_without_response_drop() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/stream",
                post_route(move || {
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
    async fn permit_is_released_when_stream_ends_with_error_without_response_drop() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/stream",
                post_route(move || {
                    let call = calls.fetch_add(1, Ordering::Relaxed);
                    async move {
                        if call == 0 {
                            Body::from_stream(stream::iter([Err::<&'static str, _>(
                                std::io::Error::other("stream failed"),
                            )]))
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
        let (_parts, mut body) = first.into_parts();
        let frame = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .expect("the stream yields one frame");
        assert!(frame.is_err());

        let second = app
            .oneshot(Request::post("/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        drop(body);
    }

    /// Router whose first response yields one data frame and then stalls
    /// forever; every later call answers with an empty body. Models the stalled
    /// upstream stream that the limit exists to keep counted.
    fn stalling_stream_router() -> Router {
        let calls = Arc::new(AtomicUsize::new(0));
        Router::new()
            .route(
                "/stream",
                post_route(move || {
                    let call = calls.fetch_add(1, Ordering::Relaxed);
                    async move {
                        if call == 0 {
                            Body::from_stream(
                                stream::once(async { Ok::<_, Infallible>("chunk") })
                                    .chain(stream::pending()),
                            )
                        } else {
                            Body::empty()
                        }
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(
                ConcurrencyLimit::new(1),
                limit_requests,
            ))
    }

    /// Drive `body` to its first data frame and assert it carries `chunk`.
    async fn expect_first_chunk(body: &mut Body) {
        let data = poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx))
            .await
            .expect("the stream yields a data frame")
            .unwrap()
            .into_data()
            .expect("the first frame is data");
        assert_eq!(data, "chunk");
    }

    #[tokio::test]
    async fn permit_stays_held_after_data_until_mid_stream_body_is_dropped() {
        let app = stalling_stream_router();

        let first = send_post(&app, "/stream").await;
        assert_eq!(first.status(), StatusCode::OK);
        let (_parts, mut body) = first.into_parts();
        expect_first_chunk(&mut body).await;

        let second = send_post(&app, "/stream").await;
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(body);
        let third = send_post(&app, "/stream").await;
        assert_eq!(third.status(), StatusCode::OK);
    }

    /// A stream that has produced data but is still in progress must keep its
    /// slot. Without this, only the terminal arms of `poll_frame` are observed:
    /// a regression that also released on `Poll::Pending` passes every other
    /// test here, and any temporarily stalled SSE stream would silently stop
    /// counting against the cap — the exact quantity this limit bounds.
    #[tokio::test]
    async fn permit_is_retained_while_an_in_progress_stream_polls_pending() {
        let app = stalling_stream_router();

        let first = send_post(&app, "/stream").await;
        assert_eq!(first.status(), StatusCode::OK);
        let (_parts, mut body) = first.into_parts();
        expect_first_chunk(&mut body).await;

        // Observe `Poll::Pending` without suspending: wrapping the inner poll in
        // `Poll::Ready` makes the `await` resolve to the inner state itself.
        // Awaiting `poll_frame` directly would hang forever instead of failing —
        // `stream::pending` never wakes the task.
        let polled = poll_fn(|cx| Poll::Ready(Pin::new(&mut body).poll_frame(cx))).await;
        assert!(
            polled.is_pending(),
            "the chained stream must still be in progress"
        );

        let second = send_post(&app, "/stream").await;
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn permit_lives_until_streaming_body_is_dropped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app =
            Router::new()
                .route(
                    "/stream",
                    post_route(move || {
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
