//! One-shot loopback callback server for OAuth logins.
//!
//! Shared by every `shunt login` flow that completes in the browser. The
//! listener is bound to loopback only. OAuth secrets are passed to the waiting
//! CLI over an in-process channel and are never rendered in the browser
//! response or written to logs.

use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
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

/// How a provider's registered redirect URI must be served.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallbackConfig {
    /// Provider name used in operator-facing errors ("Claude", "Antigravity").
    pub label: &'static str,
    /// `0` picks an ephemeral port. A provider whose OAuth client registers a
    /// fixed redirect URI must pin the port it registered.
    pub port: u16,
    /// Path component of the registered redirect URI.
    pub path: &'static str,
    /// Host to advertise in the redirect URI. Use `127.0.0.1` unless the
    /// provider registered the `localhost` spelling — see [`CallbackServer::bind`].
    pub host: &'static str,
}

impl CallbackConfig {
    /// The default shape: ephemeral port, `/callback`, IPv4 literal.
    pub(crate) const fn ephemeral(label: &'static str) -> Self {
        Self {
            label,
            port: 0,
            path: "/callback",
            host: "127.0.0.1",
        }
    }
}

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
            .expect("OAuth callback lock poisoned")
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

/// A one-shot OAuth callback listener bound exclusively to loopback.
pub(crate) struct CallbackServer {
    config: CallbackConfig,
    addr: SocketAddr,
    receiver: Option<oneshot::Receiver<CallbackResult>>,
    shutdown: Vec<oneshot::Sender<()>>,
    tasks: Vec<JoinHandle<std::io::Result<()>>>,
}

impl CallbackServer {
    /// Bind the callback listener.
    ///
    /// On a fixed port the IPv6 loopback is bound too. That pairs with
    /// advertising the `localhost` hostname: RFC 6761 requires it to resolve
    /// to loopback, but it may resolve to `::1` first, and a v4-only listener
    /// would then leave the browser hanging until the login timeout. A host
    /// with no IPv6 loopback at all still works over v4 (best-effort — that
    /// bind failure only means "unavailable"). But a port already held by
    /// another process on `[::1]` is refused loudly rather than silently
    /// falling back to v4: that other process would otherwise receive the
    /// redirect instead of this listener. Providers that register the
    /// `127.0.0.1` literal do not need this, and an ephemeral port cannot use
    /// it — the two families would get different ports, so only the
    /// advertised one would receive the redirect.
    pub(crate) async fn bind(
        config: CallbackConfig,
        expected_state: String,
    ) -> anyhow::Result<Self> {
        let label = config.label;
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, config.port))
            .await
            .with_context(|| {
                format!(
                    "failed to bind {label} OAuth callback to 127.0.0.1:{}",
                    config.port
                )
            })?;
        let addr = listener
            .local_addr()
            .with_context(|| format!("failed to read {label} OAuth callback address"))?;

        let (sender, receiver) = oneshot::channel();
        let state = CallbackState {
            expected_state,
            sender: Arc::new(Mutex::new(Some(sender))),
        };

        let mut server = Self {
            config,
            addr,
            receiver: Some(receiver),
            shutdown: Vec::new(),
            tasks: Vec::new(),
        };
        server.serve(listener, state.clone());

        if config.port != 0 {
            match tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, addr.port())).await {
                Ok(listener_v6) => server.serve(listener_v6, state),
                // Another process already owns [::1]:port: silently continuing
                // would let that process receive the redirect whenever the
                // browser resolves the advertised `localhost` host to IPv6
                // first, while this listener waits forever. Loud beats a hang.
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    return Err(error).with_context(|| {
                        format!(
                            "{label} OAuth callback port {} is already in use on [::1] by \
                             another process. Free it and try again — a stray listener there \
                             can silently swallow the redirect if the browser resolves \
                             `localhost` to IPv6 first.",
                            addr.port()
                        )
                    });
                }
                // Best effort: a host without IPv6 loopback still works over v4.
                Err(error) => {
                    tracing::debug!(
                        "{label} OAuth callback could not bind [::1]:{}: {error}",
                        addr.port()
                    );
                }
            }
        }

        Ok(server)
    }

    fn serve(&mut self, listener: tokio::net::TcpListener, state: CallbackState) {
        let app = Router::new()
            .route(self.config.path, get(callback))
            .with_state(state);
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });
        self.shutdown.push(shutdown);
        self.tasks.push(task);
    }

    pub(crate) fn redirect_uri(&self) -> String {
        format!(
            "http://{}:{}{}",
            self.config.host,
            self.addr.port(),
            self.config.path
        )
    }

    #[cfg(test)]
    fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub(crate) async fn wait_for_code(mut self, wait: Duration) -> anyhow::Result<String> {
        let label = self.config.label;
        let receiver = self
            .receiver
            .take()
            .expect("OAuth callback receiver already consumed");
        let result = tokio::time::timeout(wait, receiver)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for {label} OAuth callback"))
            .and_then(|received| {
                received.map_err(|_| {
                    anyhow::anyhow!(
                        "{label} OAuth callback server stopped before receiving authorization"
                    )
                })
            })
            .and_then(|result| result);
        self.shutdown();
        for task in std::mem::take(&mut self.tasks) {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) if result.is_ok() => {
                    return Err(error)
                        .with_context(|| format!("{label} OAuth callback server failed"));
                }
                Err(error) if result.is_ok() => {
                    return Err(error)
                        .with_context(|| format!("{label} OAuth callback server task failed"));
                }
                _ => {}
            }
        }
        result
    }

    fn shutdown(&mut self) {
        for shutdown in std::mem::take(&mut self.shutdown) {
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

    const CLAUDE: CallbackConfig = CallbackConfig::ephemeral("Claude");

    #[tokio::test]
    async fn matching_callback_returns_code_and_shuts_down() {
        let server = CallbackServer::bind(CLAUDE, "expected-state".to_string())
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
        let server = CallbackServer::bind(CLAUDE, "expected-state".to_string())
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
        let server = CallbackServer::bind(CLAUDE, "expected-state".to_string())
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
        let server = CallbackServer::bind(CLAUDE, "expected-state".to_string())
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

    #[tokio::test]
    async fn a_custom_path_and_host_shape_the_redirect_uri() {
        let config = CallbackConfig {
            label: "Antigravity",
            // Port 0 still exercises path/host propagation without racing a
            // fixed port against whatever else the test host is running.
            port: 0,
            path: "/oauth-callback",
            host: "localhost",
        };
        let server = CallbackServer::bind(config, "state".to_string())
            .await
            .unwrap();
        let uri = server.redirect_uri();
        assert!(
            uri.starts_with("http://localhost:"),
            "unexpected uri: {uri}"
        );
        assert!(uri.ends_with("/oauth-callback"), "unexpected uri: {uri}");

        // The advertised path is the one that is actually served: the default
        // /callback must not answer here.
        let base = format!("http://127.0.0.1:{}", server.addr().port());
        let waiting = tokio::spawn(server.wait_for_code(Duration::from_secs(2)));
        let wrong = reqwest::get(format!("{base}/callback?code=c&state=state"))
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::NOT_FOUND);
        let right = reqwest::get(format!("{base}/oauth-callback?code=c&state=state"))
            .await
            .unwrap();
        assert_eq!(right.status(), StatusCode::OK);
        assert_eq!(waiting.await.unwrap().unwrap(), "c");
    }

    #[tokio::test]
    async fn a_fixed_port_with_v6_already_taken_refuses_loudly() {
        // Another process squatting on [::1]:port must not be silently
        // tolerated: the advertised `localhost` host can resolve there first,
        // and a quiet fallback to v4-only would leave the browser's redirect
        // swallowed by the other process instead of reaching this listener.
        let v4 = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = v4.local_addr().unwrap().port();
        drop(v4);
        let Ok(v6_holder) = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await else {
            // No IPv6 loopback on this host: nothing to assert.
            return;
        };

        let config = CallbackConfig {
            label: "Antigravity",
            port,
            path: "/oauth-callback",
            host: "localhost",
        };
        let result = CallbackServer::bind(config, "state".to_string()).await;
        let Err(error) = result else {
            panic!("a port already held on [::1] must be refused, not silently ignored");
        };
        let message = format!("{error:#}");
        assert!(
            message.contains(&port.to_string()),
            "error should name the port: {message}"
        );
        // `message.contains(&port.to_string())` alone would also pass if a
        // race let another process grab the v4 port between the `drop(v4)`
        // above and `CallbackServer::bind` — that failure names the port too,
        // but on the wrong (v4) branch. Require text unique to the intended
        // v6-already-taken branch so the assertion can't be satisfied by the
        // wrong failure.
        assert!(
            message.contains("[::1]"),
            "error should name the v6 branch, not a v4 bind race: {message}"
        );
        drop(v6_holder);
    }

    #[tokio::test]
    async fn a_fixed_port_is_reachable_over_both_loopback_families() {
        // Binding a fixed port also binds ::1 so that advertising `localhost`
        // cannot hang when the browser resolves it to IPv6 first.
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = CallbackConfig {
            label: "Antigravity",
            port,
            path: "/oauth-callback",
            host: "localhost",
        };
        let Ok(server) = CallbackServer::bind(config, "state".to_string()).await else {
            // The port was taken between probing and binding; nothing to assert.
            return;
        };
        let waiting = tokio::spawn(server.wait_for_code(Duration::from_secs(2)));
        let response = reqwest::get(format!(
            "http://[::1]:{port}/oauth-callback?code=v6-code&state=state"
        ))
        .await;
        match response {
            Ok(response) => {
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(waiting.await.unwrap().unwrap(), "v6-code");
            }
            Err(_) => {
                // No IPv6 loopback on this host: the v4 listener must still work.
                let response = reqwest::get(format!(
                    "http://127.0.0.1:{port}/oauth-callback?code=v4-code&state=state"
                ))
                .await
                .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(waiting.await.unwrap().unwrap(), "v4-code");
            }
        }
    }
}
