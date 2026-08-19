//! End-to-end checks on the `shunt gateway` command surface, run against the
//! built binary so they cover the process's real stdout rather than a library
//! return value.

use std::{
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

// The `apiKeyHelper` output contract enforced by Claude Code 2.1.234 — it trims
// stdout, then rejects the value outright if it holds a line break, a NUL, any
// space or tab, any other control character, or any byte above 126, up to 16384
// characters — is imported from the production module rather than restated here.
// It used to live only in this file, where it asserted a rule nothing enforced;
// a second copy is exactly how that drift started.
use shunt::auth::gateway::auth::{is_helper_safe, MAX_HELPER_OUTPUT};

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
        // Far-future expiry: the token is served from disk, so the command
        // makes no network call and the gateway URL is never contacted.
        self.session_at("https://gateway.example", access_token, 4_000_000_000_000)
    }

    fn session_at(&self, gateway_url: &str, access_token: &str, expires_at: i64) -> PathBuf {
        let path = self.0.join("session.json");
        let document = serde_json::json!({
            "gatewaySession": {
                "gatewayUrl": gateway_url,
                "accessToken": access_token,
                "refreshToken": "refresh-1",
                "expiresAt": expires_at
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
fn a_global_config_before_the_subcommand_is_refused_by_the_dispatcher() {
    // Covers the `cli.config.as_deref()` argument at the `Command::Gateway`
    // dispatch site, which no other test reaches: the unit test calls
    // `gateway(...)` directly and re-derives that argument itself, so mutating
    // the dispatcher to pass `None` left the whole suite green.
    let output = shunt(
        None,
        &[
            "--config",
            "/tmp/nope.toml",
            "gateway",
            "claude",
            "-p",
            "hi",
        ],
    );
    assert!(
        !output.status.success(),
        "a swallowed --config must abort rather than launch claude without it"
    );
    assert!(
        stderr(&output).contains("is not used by `shunt gateway claude`"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn a_config_after_the_subcommand_is_forwarded_rather_than_refused() {
    // The trailing-var-arg list captures it, so it is claude's flag and shunt
    // must not claim it. This is the direction guard: a refusal keyed on the
    // string rather than on what clap actually consumed would fail here.
    let output = shunt(
        None,
        &[
            "gateway",
            "claude",
            "-p",
            "hi",
            "--config",
            "/tmp/nope.toml",
        ],
    );
    assert!(!output.status.success(), "there is still no session");
    assert!(
        stderr(&output).contains("no gateway session at"),
        "it must fail on the missing session, not on the flag: {}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("is not used by"),
        "a flag clap never consumed must not be refused: {}",
        stderr(&output)
    );
}

/// A non-loopback plain-http gateway whose refresh fails fast — but only
/// *after* the plaintext warning, which is the point.
///
/// `0.0.0.0` rather than a loopback address, and that is load-bearing:
/// `Ipv4Addr::is_loopback` covers only 127.0.0.0/8, so `is_plaintext_gateway`
/// returns true here and the warning path under test is actually reached. A
/// `127.0.0.1` fixture would silently stop exercising it.
///
/// `0.0.0.0` rather than a reserved name such as `internal.invalid`, because a
/// name consults the resolver: `.invalid` never resolves, but RFC 6761 only
/// says resolvers *should* answer it locally, so an offline or sandboxed host
/// with no reachable resolver can stall until `NETWORK_TIMEOUT` instead of
/// failing fast. A connect to `0.0.0.0` is routed to localhost and port 1 is
/// never bound, so it is refused immediately with no name lookup at all.
const PLAINTEXT_GATEWAY: &str = "http://0.0.0.0:1";

#[test]
fn a_plaintext_refresh_warns_on_stderr_and_keeps_stdout_empty() {
    // The login-time warning promises the exposure continues "on every token
    // refresh for as long as the session lives". Without this the code makes
    // that promise and never keeps it.
    let dir = TempDir::new("plaintext-refresh");
    let path = dir.session_at(PLAINTEXT_GATEWAY, "access-1", 1);

    let output = shunt(Some(&path), &["gateway", "token"]);
    assert!(
        stderr(&output).contains("plain HTTP"),
        "a refresh over plaintext must warn: {}",
        stderr(&output)
    );
    // stdout is the apiKeyHelper contract: a warning there breaks
    // authentication rather than informing anyone.
    assert_eq!(stdout(&output), "");
}

#[test]
fn a_cached_token_is_served_without_repeating_the_plaintext_warning() {
    // The frequency constraint: the fast path makes no network call, so
    // warning there would fire on every single helper invocation instead of
    // roughly once per token lifetime.
    let dir = TempDir::new("plaintext-cached");
    let path = dir.session_at(PLAINTEXT_GATEWAY, "access-1", 4_000_000_000_000);

    let output = shunt(Some(&path), &["gateway", "token"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "access-1\n");
    assert!(
        !stderr(&output).contains("plain HTTP"),
        "serving a cached token performs no refresh, so it must not warn: {}",
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
