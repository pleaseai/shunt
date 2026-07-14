//! One-shot loopback callback server for Claude OAuth login.
//!
//! The listener is deliberately bound to IPv4 loopback only. OAuth secrets are
//! passed to the waiting CLI over an in-process channel and are never rendered in
//! the browser response or written to logs.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use axum::{
    extract::{rejection::QueryRejection, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use tokio::{sync::oneshot, task::JoinHandle};

const SUCCESS_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Authorization received</title></head><body><main><h1>Authorization received</h1><p>Authorization received — you can close this tab.</p></main></body></html>";
const ERROR_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Authorization failed</title></head><body><main><h1>Authorization failed</h1><p>Return to the terminal and try again.</p></main></body></html>";

type CallbackResult = anyhow::Result<String>;
type CallbackSender = oneshot::Sender<CallbackResult>;

#[derive(Clone)]
struct CallbackState {
    expected_state: String,
    sender: Arc<Mutex<Option<CallbackSender>>>,
}

impl CallbackState {
    fn complete(&self, result: CallbackResult) -> bool {
        self.sender
            .lock()
            .expect("Claude OAuth callback lock poisoned")
            .take()
            .is_some_and(|sender| sender.send(result).is_ok())
    }
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
}

async fn callback(
    State(callback): State<CallbackState>,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Response {
    // A malformed request or a state mismatch must NOT cancel the pending login.
    // The loopback port can receive stray hits (browser probes, extensions, port
    // scanners); completing the channel with an error on the first such hit would
    // abort a legitimate login. Reject them with BAD_REQUEST and keep waiting for a
    // request that carries the expected state, bounded by the caller's timeout.
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, Html(ERROR_PAGE)).into_response();
        }
    };
    if query.code.is_empty() || query.state != callback.expected_state {
        return (StatusCode::BAD_REQUEST, Html(ERROR_PAGE)).into_response();
    }
    if !callback.complete(Ok(query.code)) {
        return (StatusCode::BAD_REQUEST, Html(ERROR_PAGE)).into_response();
    }
    (StatusCode::OK, Html(SUCCESS_PAGE)).into_response()
}

/// A one-shot OAuth callback listener bound exclusively to `127.0.0.1`.
pub(crate) struct CallbackServer {
    addr: SocketAddr,
    receiver: Option<oneshot::Receiver<CallbackResult>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
}

impl CallbackServer {
    pub(crate) async fn bind(expected_state: String) -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("failed to bind Claude OAuth callback to 127.0.0.1")?;
        let addr = listener
            .local_addr()
            .context("failed to read Claude OAuth callback address")?;
        let (sender, receiver) = oneshot::channel();
        let state = CallbackState {
            expected_state,
            sender: Arc::new(Mutex::new(Some(sender))),
        };
        let app = Router::new()
            .route("/callback", get(callback))
            .with_state(state);
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });
        Ok(Self {
            addr,
            receiver: Some(receiver),
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    pub(crate) fn redirect_uri(&self) -> String {
        // Use the IPv4 loopback literal, not the `localhost` hostname: the listener
        // binds 127.0.0.1 only, and RFC 8252 §7.3 recommends the IP literal so the
        // browser's redirect can't resolve `localhost` to ::1 (where nothing listens)
        // and silently hang until the callback timeout.
        format!("http://127.0.0.1:{}/callback", self.addr.port())
    }

    #[cfg(test)]
    fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub(crate) async fn wait_for_code(mut self, wait: Duration) -> anyhow::Result<String> {
        let receiver = self
            .receiver
            .take()
            .expect("Claude OAuth callback receiver already consumed");
        let result = tokio::time::timeout(wait, receiver)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for Claude OAuth callback"))
            .and_then(|received| {
                received.map_err(|_| {
                    anyhow::anyhow!(
                        "Claude OAuth callback server stopped before receiving authorization"
                    )
                })
            })
            .and_then(|result| result);
        self.shutdown();
        if let Some(task) = self.task.take() {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) if result.is_ok() => {
                    return Err(error).context("Claude OAuth callback server failed");
                }
                Err(error) if result.is_ok() => {
                    return Err(error).context("Claude OAuth callback server task failed");
                }
                _ => {}
            }
        }
        result
    }

    fn shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn matching_callback_returns_code_and_shuts_down() {
        let server = CallbackServer::bind("expected-state".to_string())
            .await
            .unwrap();
        assert_eq!(
            server.addr().ip(),
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert!(
            server.redirect_uri().starts_with("http://127.0.0.1:"),
            "redirect_uri must advertise the IPv4 loopback literal, not localhost"
        );
        let url = format!(
            "http://127.0.0.1:{}/callback?code=callback-code&state=expected-state",
            server.addr().port()
        );
        let waiting = tokio::spawn(server.wait_for_code(Duration::from_secs(2)));
        let response = reqwest::get(url).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("Authorization received"));
        assert!(!body.contains("callback-code"));
        assert!(!body.contains("expected-state"));
        assert_eq!(waiting.await.unwrap().unwrap(), "callback-code");
    }

    #[tokio::test]
    async fn wait_for_code_times_out_without_a_callback() {
        let server = CallbackServer::bind("expected-state".to_string())
            .await
            .unwrap();
        // No request ever reaches /callback, so the receiver never resolves and
        // the wait must hit the timeout branch rather than hang.
        let error = server
            .wait_for_code(Duration::from_millis(20))
            .await
            .expect_err("no callback arrives, so the wait must time out");
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn mismatched_state_is_rejected_but_keeps_waiting() {
        let server = CallbackServer::bind("expected-state".to_string())
            .await
            .unwrap();
        let url_wrong = format!(
            "http://127.0.0.1:{}/callback?code=callback-code&state=wrong-state",
            server.addr().port()
        );
        let url_right = format!(
            "http://127.0.0.1:{}/callback?code=callback-code&state=expected-state",
            server.addr().port()
        );
        let waiting = tokio::spawn(server.wait_for_code(Duration::from_secs(2)));
        // A stray request with the wrong state is rejected without exposing secrets
        // and, crucially, must not cancel the pending login.
        let response = reqwest::get(url_wrong).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.text().await.unwrap();
        assert!(!body.contains("callback-code"));
        assert!(!body.contains("wrong-state"));
        // The subsequent legitimate callback still completes the flow.
        let response = reqwest::get(url_right).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(waiting.await.unwrap().unwrap(), "callback-code");
    }

    #[tokio::test]
    async fn malformed_query_is_rejected_but_keeps_waiting() {
        let server = CallbackServer::bind("expected-state".to_string())
            .await
            .unwrap();
        // Missing `code`/`state` params (a QueryRejection) must also be rejected
        // without cancelling the pending login.
        let url_bad = format!(
            "http://127.0.0.1:{}/callback?code=callback-code",
            server.addr().port()
        );
        let url_right = format!(
            "http://127.0.0.1:{}/callback?code=callback-code&state=expected-state",
            server.addr().port()
        );
        let waiting = tokio::spawn(server.wait_for_code(Duration::from_secs(2)));
        let response = reqwest::get(url_bad).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = reqwest::get(url_right).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(waiting.await.unwrap().unwrap(), "callback-code");
    }
}
