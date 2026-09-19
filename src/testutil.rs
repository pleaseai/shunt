#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Test-only harnesses shared by inline test modules.

/// A raw one-shot HTTP responder for a terminal error status whose body
/// stalls part-way: the headers (and with them the client's send) resolve
/// immediately, and the declared content-length completes only when the test
/// releases the body. wiremock cannot express the split — its response delay
/// covers headers and body alike — so tests pinning post-header behaviour
/// (deferred body reads, header-arrival latency samples) drive this instead.
pub(crate) struct StalledBody {
    pub(crate) base_url: String,
    release: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl StalledBody {
    /// Serve exactly one request: `status`, then `prefix` bytes of the body,
    /// parked until [`StalledBody::release`] completes the declared length
    /// with `suffix`.
    pub(crate) async fn start(
        status: axum::http::StatusCode,
        prefix: &'static str,
        suffix: &'static str,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                        status.as_str(),
                        prefix.len() + suffix.len(),
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(prefix.as_bytes()).await.unwrap();
            let _ = released.await;
            socket.write_all(suffix.as_bytes()).await.unwrap();
        });
        Self {
            base_url: format!("http://{addr}"),
            release,
            task,
        }
    }

    /// Release the stalled body and wait for the response to complete.
    pub(crate) async fn release(self) {
        let _ = self.release.send(());
        let _ = self.task.await;
    }
}
