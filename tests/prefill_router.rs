//! `[models.router] type = "prefill_router"` end to end (ADR-0005 §6, issue #597).
//!
//! The split here is the point of the file. The unit tests in
//! `src/config/router/prefill.rs`, `src/config.rs`, and `src/routing/prefill/`
//! pin the schema, the verdicts, and the driven decision against a stub. What
//! only an integration test can prove is what an *operator's* build does with
//! the same TOML:
//!
//! * without the feature — the shipped shape of every release binary — the
//!   config still parses and `Config::load` refuses it by naming the cargo
//!   feature, so the failure is actionable rather than "unknown variant";
//! * with the feature, the same TOML loads;
//! * with the feature *and* a real checkpoint plus an importable torch, the
//!   gateway routes a live request through libsy and stamps the route source.
//!
//! The last one needs assets and a Python environment no CI runner has, so it
//! skips loudly rather than being `#[ignore]`d — an ignored test is invisible
//! in a run, and this one has to say out loud that it did not prove anything.

#![allow(clippy::items_after_test_module)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod common;

/// A temp directory holding one `shunt.toml`, modelled on `tests/check_cli.rs`.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "shunt-prefill-router-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp directory");
        Self(path)
    }

    fn config(&self, config: &str) -> PathBuf {
        let path = self.0.join("shunt.toml");
        std::fs::write(&path, config).expect("write config");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One router entry over two ordinary aliases, with `checkpoint` pointing at
/// `checkpoint`. Nothing opens that path during `Config::load`.
fn config_toml(checkpoint: &Path) -> String {
    format!(
        r#"
[[models]]
id = "claude-prefill"

[models.router]
type = "prefill_router"
targets = ["claude-sonnet-4-6", "claude-opus-4-8"]
checkpoint = "{}"

[[models]]
id = "claude-sonnet-4-6"
[models.upstream_model]
codex = "gpt-5.2"

[[models]]
id = "claude-opus-4-8"
[models.upstream_model]
codex = "gpt-5.2-codex"
"#,
        checkpoint.display()
    )
}

/// The load path, not just `validate()`: an operator meets this through
/// `shunt check` / `shunt run`, and the remedy has to survive to there.
#[cfg(not(feature = "prefill-router"))]
#[test]
fn a_prefill_router_entry_is_refused_by_name_when_the_feature_is_off() {
    // `TempDir::new` reads `TMPDIR` and `Config::load` reads the environment
    // while resolving its own paths, so this test is an environment *reader*
    // and takes the shared guard for its whole body (`tests/AGENTS.md`).
    let _env = common::set_env_blocking(&[]);
    let dir = TempDir::new("off");
    let path = dir.config(&config_toml(Path::new("/models/router.pt")));

    let error = shunt::config::Config::load(Some(&path))
        .expect_err("a release binary cannot serve a prefill_router entry");
    let rendered = error.to_string();

    assert!(
        rendered.contains("prefill-router"),
        "the rejection must name the cargo feature, got: {rendered}"
    );
    assert!(
        rendered.contains("claude-prefill"),
        "the rejection must name the entry, got: {rendered}"
    );
}

/// The positive twin: the same bytes load under the feature. No Python is
/// needed — `load` validates and stops there.
#[cfg(feature = "prefill-router")]
#[test]
fn a_prefill_router_entry_validates_when_the_feature_is_on() {
    let _env = common::set_env_blocking(&[]);
    let dir = TempDir::new("on");
    let path = dir.config(&config_toml(Path::new("/models/router.pt")));

    shunt::config::Config::load(Some(&path)).expect("the entry loads under the feature");
}

/// The live lane. Needs a checkpoint and an importable torch/transformers, so
/// it announces a skip rather than failing on a machine that has neither.
#[cfg(feature = "prefill-router")]
#[tokio::test]
async fn a_prefill_router_entry_loads_and_routes_through_libsy() {
    // Taken before `live_checkpoint`, which reads `SHUNT_PREFILL_ROUTER_CHECKPOINT`
    // and `PYO3_PYTHON`: the guard has to cover the reads that decide the skip,
    // not just the ones after it. Async because `set_env_blocking` panics inside
    // a runtime.
    let _env = common::env_lock().await;
    let Some(checkpoint) = live_checkpoint() else {
        return;
    };
    let dir = TempDir::new("live");
    let path = dir.config(&config_toml(&checkpoint));
    let config = shunt::config::Config::load(Some(&path)).expect("the live config loads");

    // Building the runtime state is what loads the checkpoint; a failure here
    // is the assertion, not an environment problem, because the skip gate
    // above already established the environment.
    let state = shunt::reload::RuntimeState::from_config(config.clone())
        .expect("the checkpoint loads into a runtime state");
    assert!(
        !state.prefill_routers.is_empty(),
        "the entry must have produced a built algorithm"
    );

    if !can_bind_loopback() {
        return;
    }
    let gateway = start_gateway(config).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "prefill-live")
        .json(&serde_json::json!({
            "model": "claude-prefill",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "write a haiku about routing"}],
        }))
        .send()
        .await
        .expect("the gateway answers");

    // The two targets map to `codex`, which this config gives no credential, so
    // the request is expected to fail *upstream* — after routing. Only the
    // stamp matters: it says the driven lane decided this turn.
    assert_eq!(
        response
            .headers()
            .get("x-gateway-route-source")
            .and_then(|value| value.to_str().ok()),
        Some("prefill"),
        "the response must be stamped by the driven lane"
    );
}

/// A gateway on an ephemeral loopback port, as `tests/noop_router.rs` starts one.
#[cfg(feature = "prefill-router")]
struct TestGateway {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(feature = "prefill-router")]
impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(feature = "prefill-router")]
fn can_bind_loopback() -> bool {
    match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping network integration test: loopback bind is not permitted");
            false
        }
        Err(error) => panic!("unexpected loopback bind failure: {error}"),
    }
}

#[cfg(feature = "prefill-router")]
async fn start_gateway(mut config: shunt::config::Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (app, _, _) = shunt::server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestGateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

/// `Some(checkpoint)` only when both preconditions hold; otherwise it prints
/// why and the caller returns.
#[cfg(feature = "prefill-router")]
fn live_checkpoint() -> Option<PathBuf> {
    let Ok(checkpoint) = std::env::var("SHUNT_PREFILL_ROUTER_CHECKPOINT") else {
        eprintln!(
            "skipping the live prefill_router test: set SHUNT_PREFILL_ROUTER_CHECKPOINT to a \
             router checkpoint to run it"
        );
        return None;
    };
    let python = std::env::var("PYO3_PYTHON").unwrap_or_else(|_| "python3".to_string());
    let importable = std::process::Command::new(&python)
        .args(["-c", "import torch, transformers"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !importable {
        eprintln!(
            "skipping the live prefill_router test: `{python} -c 'import torch, transformers'` \
             did not succeed"
        );
        return None;
    }
    Some(PathBuf::from(checkpoint))
}
