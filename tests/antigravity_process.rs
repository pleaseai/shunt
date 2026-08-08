//! End-to-end coverage of the Antigravity adapter's *process* path.
//!
//! `tests/antigravity_translate.rs` covers the pure pieces — prompt building,
//! event translation, workspace resolution. Nothing covered the part that
//! spawns `agy` and pumps its stdout, which is where both of this branch's
//! review findings lived: a per-line timeout that never bounded the turn, and
//! a terminal path that reaped a child it had not killed.
//!
//! `find_agy_binary()` checks `AGY_BIN` first, so a stub script standing in for
//! the CLI drives the real adapter without the real Antigravity install.
//!
//! **This must stay its own test target.** `find_agy_binary()` memoizes in a
//! `OnceLock`, and `antigravity_translate.rs` already sets `AGY_BIN` to a temp
//! path *and then deletes it*. Sharing that process would leave this file
//! spawning a binary that no longer exists. A separate `tests/` file is a
//! separate binary, hence a separate process and a fresh `OnceLock`.
#![cfg(unix)]

use std::{
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use serde_json::Value;
use shunt::{config::Config, server};
use tokio::task::JoinHandle;

/// Model id the fixture config routes to the stub.
const MODEL: &str = "agy-test-model";

/// Ceiling for a whole request. Generous for a stub that answers instantly,
/// but far below the adapter's own timeouts, so a regression that hangs fails
/// the assertion instead of stalling CI until the job limit.
const REQUEST_GUARD: Duration = Duration::from_secs(20);

/// Stub `agy`, written once per test binary.
///
/// Behaviour is selected by markers in the prompt rather than by environment
/// variables: `std::env::set_var` is process-global and these tests share a
/// process, so per-test env mutation would race. All env writes happen inside
/// this initializer, which runs exactly once.
fn stub_agy() -> &'static Path {
    static STUB: OnceLock<PathBuf> = OnceLock::new();
    STUB.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("shunt-agy-stub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("agy");
        std::fs::write(&script, STUB_SCRIPT).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        std::env::set_var("AGY_BIN", &script);
        // Pin the workspace instead of falling back to the test process's cwd,
        // so a stub that misbehaves cannot touch the checkout.
        std::env::set_var("SHUNT_AGY_WORKSPACE", &dir);
        // Resolve now, while this initializer still holds the only writer, so
        // the memoized value is this stub for every later call.
        let resolved = shunt::adapters::antigravity::find_agy_binary()
            .expect("AGY_BIN should resolve to the stub");
        assert_eq!(resolved, script);
        script
    })
    .as_path()
}

/// POSIX `sh` only — CI measures coverage on Linux, and this must not depend
/// on the real `agy`, GNU coreutils, or bash builtins.
const STUB_SCRIPT: &str = r#"#!/bin/sh
# `models` is the discovery call models::effort_matrix makes against this same
# binary; answer it so effort resolution behaves as it would in production.
if [ "$1" = "models" ]; then
  printf 'gemini-3.1-pro-high\ngemini-3.1-pro-low\n'
  exit 0
fi

prompt=
prev=
for arg in "$@"; do
  if [ "$prev" = "-p" ]; then prompt=$arg; fi
  prev=$arg
done

case "$prompt" in
  *MODE=fail*)
    echo "stub diagnostic on stderr" >&2
    exit 1
    ;;
  *MODE=eof-hang*)
    printf '%s\n' '{"event":"init","init":{"model":"gemini-3.1-pro"}}'
    printf '%s\n' '{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"partial"}}'
    # Close stdout without a result event, then stay alive. `exec` replaces the
    # shell so the adapter's kill() reaps the sleeper itself rather than an
    # intermediate that leaves it orphaned.
    exec 1>&-
    exec sleep 300
    ;;
  *)
    printf '%s\n' '{"event":"init","init":{"model":"gemini-3.1-pro"}}'
    printf '%s\n' '{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"hello","usage":{"input_tokens":10,"output_tokens":3}}}'
    printf '%s\n' '{"event":"result","result":{"status":"SUCCESS","response":"hello","usage":{"input_tokens":10,"output_tokens":3}}}'
    ;;
esac
"#;

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Sandboxed CI can forbid binding even a loopback port; skip rather than fail.
fn can_bind_loopback() -> bool {
    std::net::TcpListener::bind("127.0.0.1:0").is_ok()
}

async fn start_gateway() -> TestGateway {
    let dir = stub_agy().parent().unwrap();
    let config_path = dir.join("shunt.toml");
    std::fs::write(
        &config_path,
        format!(
            concat!(
                "[server]\n",
                "bind = \"127.0.0.1:0\"\n",
                "default_provider = \"agy\"\n\n",
                "[providers.agy]\n",
                "kind = \"antigravity\"\n",
                "base_url = \"http://localhost\"\n",
                "auth = \"none\"\n\n",
                "[[routes]]\n",
                "model = \"{model}\"\n",
                "provider = \"agy\"\n",
                "upstream_model = \"gemini-3.1-pro\"\n",
            ),
            model = MODEL
        ),
    )
    .unwrap();

    let mut config = Config::load(Some(&config_path)).expect("fixture config should load");
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _, _) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    TestGateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

/// Send a turn and read the body to completion.
///
/// The guard wraps the body read as well as `send()`: a streaming response
/// resolves as soon as headers arrive, so a timeout around `send()` alone
/// would sail straight past a hang in the body stream — which is exactly the
/// regression `streaming_premature_eof_does_not_hang` exists to catch.
async fn turn(gateway: &TestGateway, marker: &str, stream: bool) -> (reqwest::StatusCode, String) {
    let body = serde_json::json!({
        "model": MODEL,
        "max_tokens": 64,
        "stream": stream,
        "messages": [{ "role": "user", "content": marker }],
    });
    let request = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .json(&body)
        .send();

    tokio::time::timeout(REQUEST_GUARD, async {
        let response = request.await.expect("request should reach the gateway");
        let status = response.status();
        let text = response.text().await.expect("body should complete");
        (status, text)
    })
    .await
    .expect("the turn must finish within the guard rather than hang")
}

#[tokio::test]
async fn streaming_turn_translates_stub_events_to_sse() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway().await;
    let (status, body) = turn(&gateway, "MODE=ok please", true).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    // Substring containment, never an exact frame sequence: the shared
    // keepalive interleaves pings nondeterministically.
    assert!(body.contains("message_start"), "body: {body}");
    assert!(body.contains("hello"), "body: {body}");
    assert!(body.contains("message_stop"), "body: {body}");
    assert!(
        !body.contains("[agy error]"),
        "a successful turn must not report an error: {body}"
    );
}

#[tokio::test]
async fn streaming_premature_eof_does_not_hang_and_reports_an_error() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway().await;
    // Regression: the stub closes stdout with no result event and then sleeps
    // for 300s. Before the fix, the terminal arm called `child.wait()` without
    // `kill()`, so the response hung behind the sleeper and this `turn` would
    // exceed REQUEST_GUARD.
    let (status, body) = turn(&gateway, "MODE=eof-hang now", true).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        body.contains("[agy error]"),
        "a premature EOF must be reported in-band, not closed silently: {body}"
    );
    assert!(body.contains("without reporting a result"), "body: {body}");
}

#[tokio::test]
async fn non_streaming_turn_returns_the_translated_message() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway().await;
    let (status, body) = turn(&gateway, "MODE=ok please", false).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("a non-streaming turn returns JSON");
    assert_eq!(json["content"][0]["text"], "hello");
    assert_eq!(json["stop_reason"], "end_turn");
    assert_eq!(json["usage"]["output_tokens"], 3);
}

#[tokio::test]
async fn non_streaming_failure_reports_the_exit_status() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway().await;
    let (status, body) = turn(&gateway, "MODE=fail now", false).await;

    assert!(
        status.is_client_error() || status.is_server_error(),
        "status: {status}"
    );
    // The exit status reaches the caller. This assertion is only satisfiable
    // because the status branch was made reachable: chained onto
    // `terminal_failure`, which never returns `None` for a missing result, it
    // was dead code and this message could never be emitted.
    assert!(
        body.contains("exited without a result (status 1)"),
        "body: {body}"
    );
    // Deliberately not asserting on stderr text: `drain_stderr` publishes into
    // a shared buffer with no join handle, so the child can be reaped before
    // the final line lands. Asserting it would be flaky.
}
