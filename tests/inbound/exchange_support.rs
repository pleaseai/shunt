//! Shared fixtures for the workload token-exchange suites: the `jwt-bearer`
//! grant URN and a real router bound to a loopback port.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use shunt::{config::Config, reload::SharedState, server};
use tokio::task::JoinHandle;

pub const GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

pub struct Server {
    pub base_url: String,
    pub shared: SharedState,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn start(mut config: Config) -> Server {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (app, shared, _state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server {
        base_url: format!("http://{addr}"),
        shared,
        task,
    }
}
