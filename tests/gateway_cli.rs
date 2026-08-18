//! End-to-end checks on the `shunt gateway` command surface, run against the
//! built binary so they cover the process's real stdout rather than a library
//! return value.

use std::{
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

/// The `apiKeyHelper` output contract enforced by Claude Code 2.1.234: it trims
/// stdout, then rejects the value outright if it holds a line break, a NUL, any
/// space or tab, any other control character, or any byte above 126 — printable
/// ASCII only — up to 16384 characters. `shunt gateway token` feeds that
/// validator, so anything shunt adds to stdout breaks authentication with no
/// useful diagnostic.
const MAX_HELPER_OUTPUT: usize = 16_384;

fn is_helper_safe(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_HELPER_OUTPUT
        && value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "shunt-gateway-cli-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp directory");
        Self(path)
    }

    fn session(&self, access_token: &str) -> PathBuf {
        let path = self.0.join("session.json");
        // Far-future expiry: the token is served from disk, so the command
        // makes no network call and the gateway URL is never contacted.
        let document = serde_json::json!({
            "gatewaySession": {
                "gatewayUrl": "https://gateway.example",
                "accessToken": access_token,
                "refreshToken": "refresh-1",
                "expiresAt": 4_000_000_000_000_i64
            }
        });
        std::fs::write(
            &path,
            serde_json::to_vec(&document).expect("serialize session"),
        )
        .expect("write session");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shunt(session: Option<&PathBuf>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shunt"));
    command.args(args);
    match session {
        Some(path) => command.env("SHUNT_GATEWAY_SESSION_FILE", path),
        None => command.env(
            "SHUNT_GATEWAY_SESSION_FILE",
            "/nonexistent/shunt/session.json",
        ),
    };
    command.output().expect("shunt binary should run")
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("stdout should be UTF-8")
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("stderr should be UTF-8")
}

#[test]
fn token_prints_the_token_and_nothing_else_on_stdout() {
    let dir = TempDir::new("token");
    // Shaped like a real gateway JWT: three dot-separated base64url segments.
    let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJkZXZAZXhhbXBsZS5jb20ifQ.c2lnbmF0dXJl-_9";
    let path = dir.session(token);

    let output = shunt(Some(&path), &["gateway", "token"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    // Exactly the token plus one newline: a banner, hint, or debug line on
    // stdout would fail Claude Code's helper validation with no diagnostic.
    assert_eq!(stdout(&output), format!("{token}\n"));
    assert!(
        is_helper_safe(stdout(&output).trim()),
        "helper output must be printable ASCII with no whitespace: {:?}",
        stdout(&output)
    );
}

#[test]
fn token_without_a_session_fails_with_an_empty_stdout() {
    let output = shunt(None, &["gateway", "token"]);
    assert!(!output.status.success());
    // The failure path must not put a word on stdout either: Claude Code reads
    // stdout regardless of the exit status.
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("shunt gateway login"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn claude_launcher_reports_a_missing_session_instead_of_launching() {
    let output = shunt(None, &["gateway", "claude", "-p", "hi"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("shunt gateway login"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn helper_safety_rule_rejects_what_claude_code_rejects() {
    // Guards the checker above from being trivially true.
    assert!(is_helper_safe("sk-ant-abc123"));
    assert!(!is_helper_safe(""));
    assert!(!is_helper_safe("token with space"));
    assert!(!is_helper_safe("token\twith-tab"));
    assert!(!is_helper_safe("token\nwith-newline"));
    assert!(!is_helper_safe("token\0with-nul"));
    assert!(!is_helper_safe("tokén-non-ascii"));
    assert!(!is_helper_safe(&"x".repeat(MAX_HELPER_OUTPUT + 1)));
}
