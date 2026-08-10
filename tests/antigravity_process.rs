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
    sync::{Arc, OnceLock},
    time::Duration,
};

use serde_json::Value;
use shunt::{
    config::{Config, RouteConfig},
    reload::RuntimeState,
    server,
};
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
        //
        // Deliberately written in a non-canonical form. The same value becomes
        // both `--add-dir` and `current_dir`, so it must be canonicalized
        // before it reaches the child; a `..` segment here keeps
        // `workspace_env_is_canonicalized_before_it_reaches_the_child`
        // non-vacuous on Linux, where the temp dir is already a real path. It
        // resolves to `dir`, so every other test sees the workspace unchanged.
        std::fs::create_dir_all(dir.join("wsprobe")).unwrap();
        std::env::set_var("SHUNT_AGY_WORKSPACE", dir.join("wsprobe").join(".."));
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
  *MODE=late-stderr*)
    # Exit immediately, but publish the diagnostic half a second later from a
    # detached subshell. Without waiting for the drain, the buffer is read at
    # ~0ms and deterministically yields nothing; with it, the line lands well
    # inside DRAIN_GRACE. This is what makes the reap-before-publish race
    # testable at all — the plain MODE=fail stub writes early enough that the
    # race almost never loses.
    # Deliberately NOT recorded as a holder: it exits on its own in 0.5s.
    # It stays inside agy's process group because surviving the non-streaming
    # stderr settle window is what makes this test discriminate.
    # stdout redirected to /dev/null: if the subshell inherited stdout too, it
    # would hold that pipe open and the adapter would still be blocked reading
    # stdout at 0.5s, so the diagnostic would land before stderr was ever read
    # and the test would pass with or without the fix. Holding ONLY stderr is
    # what makes this discriminate.
    ( sleep 0.5; echo "late diagnostic" >&2 ) >/dev/null &
    exit 1
    ;;
  *MODE=unterminated-stderr*)
    # ~256 KiB of stderr with no newline anywhere, then a real diagnostic.
    # Line-oriented draining accumulated the whole run of bytes before any size
    # check could see it, so a child that never emits `\n` grew the buffer
    # without bound. Only a bounded prefix may be retained.
    i=0
    while [ $i -lt 256 ]; do
      printf 'PADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPAD' >&2
      i=$((i + 1))
    done
    exit 1
    ;;
  *MODE=bad-utf8-stderr*)
    # A byte sequence that is never valid UTF-8, then a real diagnostic. Reading
    # stderr as lines made the first one an `Err`, which ended the drain and
    # lost everything after it — and, worse, left the child free to block on a
    # full pipe. Both bytes must be consumed and the later line still delivered.
    printf '\376\377 binary junk\n' >&2
    printf 'valid diagnostic after junk\n' >&2
    exit 1
    ;;
  *MODE=fail*)
    echo "stub diagnostic on stderr" >&2
    exit 1
    ;;
  *MODE=result-then-hold*)
    tag=${prompt##*TAG=}
    tag=${tag%% *}
    # Start the pipe holder and record its pid FIRST. The adapter stops at the
    # result and kills this stub immediately, so anything after that print can
    # be cut off mid-script — recording the pid afterwards loses the race and
    # orphans a 300s process with no way to reap it.
    #
    # The holder is a GRANDCHILD and inherits the agy process group. The adapter
    # must kill that whole group when the turn finishes; the pid file lets the
    # test prove it and clean up safely if containment regresses.
    sleep 300 &
    echo $! > "$(dirname "$0")/holder-$tag.pid"
    # Terminal result, then the descendant still holds stdout. The turn is
    # finished; only the pipe is open. A reader that waits for EOF instead of
    # stopping at the result stalls here until its deadline.
    printf '%s\n' '{"event":"init","init":{"model":"gemini-3.1-pro"}}'
    printf '%s\n' '{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"hello","usage":{"input_tokens":10,"output_tokens":3}}}'
    printf '%s\n' '{"event":"result","result":{"status":"SUCCESS","response":"hello","usage":{"input_tokens":10,"output_tokens":3}}}'
    exit 0
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

async fn start_gateway_after_unsandboxed_loopback_reload() -> TestGateway {
    let _ = stub_agy();
    let mut boot_config = Config::default();
    boot_config.server.bind = "0.0.0.0:0".to_string();
    boot_config.server.default_provider = "antigravity".to_string();
    boot_config.routes.push(RouteConfig {
        model: MODEL.to_string(),
        provider: "antigravity".to_string(),
        upstream_model: Some("gemini-3.1-pro".to_string()),
        effort: None,
        service_tier: None,
    });

    // build_router records the public boot bind but does not open it. The only
    // socket this test creates remains loopback-only.
    let mut reloaded_config = boot_config.clone();
    let (app, shared, _) = server::build_router(boot_config).unwrap();
    reloaded_config.server.bind = "127.0.0.1:0".to_string();
    reloaded_config
        .providers
        .get_mut("antigravity")
        .unwrap()
        .sandbox = false;
    let runtime = RuntimeState::from_config(reloaded_config).unwrap();
    shared.store(Arc::new(runtime));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
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
async fn reload_cannot_unsandbox_antigravity_on_the_public_boot_listener() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway_after_unsandboxed_loopback_reload().await;
    let (status, body) = turn(&gateway, "MODE=ok must not run", false).await;

    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("unsandboxed"), "body: {body}");
    assert!(
        body.contains("running listener is non-loopback"),
        "body: {body}"
    );
    assert!(
        body.contains("reload cannot move the listener"),
        "body: {body}"
    );
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

/// Per-turn safety net for a pipe-holding grandchild.
///
/// The adapter should kill the process before the turn returns. Drop remains
/// responsible for only this test's tagged pid if containment regresses or an
/// assertion unwinds early.
struct HolderGuard {
    path: PathBuf,
}

impl HolderGuard {
    fn new(tag: &str) -> Self {
        let path = stub_agy()
            .parent()
            .unwrap()
            .join(format!("holder-{tag}.pid"));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    fn pid(&self) -> libc::pid_t {
        std::fs::read_to_string(&self.path)
            .expect("the stub should record its holder pid")
            .trim()
            .parse()
            .expect("the holder pid should be numeric")
    }
}

impl Drop for HolderGuard {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(&self.path) {
            if let Ok(pid) = pid.trim().parse::<libc::pid_t>() {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn assert_process_exited(pid: libc::pid_t) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        // Signal 0 also succeeds for an unreaped zombie. Linux exposes the
        // process state independently of PID 1's reaping cadence, so treat Z as
        // exited; the comm field can contain spaces, hence the last `)` split.
        #[cfg(target_os = "linux")]
        let alive = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                stat.rsplit_once(')')
                    .and_then(|(_, suffix)| suffix.trim_start().chars().next())
                    != Some('Z')
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => unsafe { libc::kill(pid, 0) == 0 },
        };
        #[cfg(not(target_os = "linux"))]
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        if !alive {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "holder process {pid} survived the completed turn"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn streaming_finishes_when_a_descendant_holds_stdout_open() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway().await;
    let holder = HolderGuard::new("stream-hold");
    // Regression: the stub emits a terminal SUCCESS result, then leaves a
    // background `sleep` holding the inherited stdout pipe. Reading to EOF
    // instead of stopping at the result stalls a *finished* turn until the
    // deadline and then reports it as a failure.
    let (status, body) = turn(&gateway, "MODE=result-then-hold TAG=stream-hold", true).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(body.contains("hello"), "body: {body}");
    assert!(body.contains("message_stop"), "body: {body}");
    assert!(
        !body.contains("[agy error]"),
        "a completed turn must not be reported as an error: {body}"
    );
    assert_process_exited(holder.pid()).await;
}

#[tokio::test]
async fn non_streaming_finishes_when_a_descendant_holds_stdout_open() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway().await;
    let holder = HolderGuard::new("non-stream-hold");
    let (status, body) = turn(&gateway, "MODE=result-then-hold TAG=non-stream-hold", false).await;

    assert_eq!(status, reqwest::StatusCode::OK);
    let json: Value = serde_json::from_str(&body).expect("a non-streaming turn returns JSON");
    assert_eq!(json["content"][0]["text"], "hello");
    assert_eq!(json["stop_reason"], "end_turn");
    assert_process_exited(holder.pid()).await;
}

#[tokio::test]
async fn invalid_utf8_on_stderr_does_not_stop_the_drain() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway().await;
    // Regression: reading stderr with `lines()` yielded `Err(InvalidData)` on
    // the junk bytes, which ended the drain loop. Everything written after it
    // was lost, and a child that kept writing would have blocked forever on a
    // pipe nobody was reading.
    let (status, body) = turn(&gateway, "MODE=bad-utf8-stderr", false).await;

    assert!(
        status.is_client_error() || status.is_server_error(),
        "status: {status}"
    );
    assert!(
        body.contains("valid diagnostic after junk"),
        "stderr written after invalid UTF-8 must still reach the caller: {body}"
    );
}

#[test]
fn workspace_env_is_canonicalized_before_it_reaches_the_child() {
    let dir = stub_agy().parent().expect("stub lives in a directory");
    // `SHUNT_AGY_WORKSPACE` is set to a `..`-bearing form of this same
    // directory. It becomes both `--add-dir` and `current_dir`, so if it
    // reached the child uncanonicalized the child would resolve the add-dir
    // from its new cwd and be granted a different directory than the one that
    // was vetted.
    let resolved = shunt::adapters::antigravity::resolve_workspace(&serde_json::json!({}), &[])
        .expect("the configured workspace resolves");

    assert_eq!(
        resolved,
        dir.canonicalize().expect("stub dir canonicalizes")
    );
    assert!(
        !resolved
            .components()
            .any(|component| component.as_os_str() == ".."),
        "no relative segment may survive into the spawned command: {}",
        resolved.display()
    );
}

#[tokio::test]
async fn newline_free_stderr_still_completes_the_turn() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway().await;
    // A child that emits no newline at all — a redrawing progress bar, or
    // binary noise — must still be drained to EOF and reported. The memory
    // bound itself is asserted in `drain_stderr_bounds_newline_free_output`;
    // this path cannot show it, because `stderr_text` truncates on the way out
    // and so returns a small body either way.
    let (status, body) = turn(&gateway, "MODE=unterminated-stderr", false).await;

    assert!(
        status.is_client_error() || status.is_server_error(),
        "status: {status}"
    );
    assert!(
        body.len() < 8 * 1024,
        "an unterminated stderr run must be truncated, not relayed whole: {} bytes",
        body.len()
    );
    assert!(
        body.contains("PAD"),
        "the retained prefix should still carry the diagnostic: {body}"
    );
}

#[tokio::test]
async fn late_stderr_still_reaches_the_caller() {
    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway().await;
    // Regression for the reap-before-publish race. The stub exits at once and
    // writes its diagnostic 0.5s later, so without waiting for the drain the
    // buffer is read empty every time — deterministically, unlike MODE=fail
    // where the diagnostic lands early enough that the race almost never loses.
    let (status, body) = turn(&gateway, "MODE=late-stderr", false).await;

    assert!(
        status.is_client_error() || status.is_server_error(),
        "status: {status}"
    );
    assert!(
        body.contains("late diagnostic"),
        "a diagnostic written after exit must still reach the caller: {body}"
    );
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
    // The CLI's own diagnostic must survive to the caller. This is only
    // assertable because the adapter now waits for the stderr drain to publish
    // before composing the message; previously the child could be reaped first
    // and the caller got "no stderr output" instead.
    assert!(
        body.contains("stub diagnostic on stderr"),
        "the CLI's stderr must reach the caller: {body}"
    );
}
