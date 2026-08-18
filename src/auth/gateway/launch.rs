//! `shunt gateway claude` — run Claude Code against a logged-in shunt gateway.
//!
//! The launcher hands `claude` an inline `--settings` document that points its
//! base URL at the gateway and wires `apiKeyHelper` back to
//! `shunt gateway token`. That document applies to the launched process only:
//! it never writes `~/.claude/settings.json`, and it overrides any
//! `apiKeyHelper` or `ANTHROPIC_BASE_URL` already configured for exactly one
//! invocation. That scoping is the reason this subcommand exists.
//!
//! Measured against the shipped Claude Code 2.1.234 binary, this keeps the
//! client in its first-party mode (12 betas negotiated, `opus`/`sonnet`
//! resolving to the `-5` models). Setting `CLAUDE_CODE_USE_GATEWAY=1` and
//! `ANTHROPIC_AUTH_TOKEN` instead fills the `gatewayAuth` credential slot and
//! degrades the client (7 betas, `opus` pinned to `claude-opus-4-7`), so this
//! module deliberately sets neither.
//!
//! Also measured against 2.1.234: the settings document's `env` block **beats
//! an ambient exported `ANTHROPIC_BASE_URL`**. With the variable exported in
//! the environment pointing at one mock server and `--settings` naming a
//! second, only the settings target ever received `POST /v1/messages`; the
//! exported one received nothing. That is what makes this subcommand safe to
//! run from a shell that already exports `ANTHROPIC_BASE_URL` for some other
//! gateway — the launcher does not need to clear the variable first, and a
//! future reader should not "fix" it by doing so.

use std::{io, path::Path, process::Command};

use anyhow::{anyhow, Context};
use serde_json::json;

use super::store;

const CLAUDE_BIN: &str = "claude";

/// Launch `claude`, forwarding `forwarded` verbatim after the settings flag.
pub fn run(forwarded: &[String]) -> anyhow::Result<()> {
    let path = store::session_path();
    // Read only: `shunt gateway token` owns refresh and rotation, and doing it
    // here as well would widen the single-use refresh-token race the helper's
    // file lock exists to close.
    let session = store::read_session(&path)?.ok_or_else(|| {
        anyhow!(
            "no gateway session at {}; run `shunt gateway login <url>` first",
            path.display()
        )
    })?;
    // The helper string is resolved by Claude Code, not by this shell, so a
    // bare `shunt` would only fail much later — as an opaque auth error — on a
    // machine where shunt is not on PATH.
    let executable = std::env::current_exe()
        .context("could not resolve the shunt executable path for the apiKeyHelper")?;
    let settings = settings_document(&session.gateway_url, &executable)?;
    exec_claude(&settings, forwarded)
}

/// The inline `--settings` document. Built through `serde_json` rather than
/// `format!` so a gateway URL or install path containing a quote or backslash
/// cannot produce a malformed document.
pub(crate) fn settings_document(
    gateway_url: &str,
    shunt_executable: &Path,
) -> anyhow::Result<String> {
    let helper = format!(
        "{} gateway token",
        shell_quote(&shunt_executable.to_string_lossy())
    );
    serde_json::to_string(&json!({
        "env": { "ANTHROPIC_BASE_URL": gateway_url },
        "apiKeyHelper": helper
    }))
    .context("failed to serialize the Claude Code settings document")
}

/// Quote a path for the shell Claude Code runs `apiKeyHelper` through. An
/// unquoted path containing a space — which happens on macOS under some install
/// layouts — would be split into a command plus an argument and fail as an
/// opaque auth error rather than as a missing file.
#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `cmd.exe` has no single-quote form, and a Windows path cannot contain a
/// double quote, so wrapping is enough there.
#[cfg(windows)]
fn shell_quote(value: &str) -> String {
    format!("\"{value}\"")
}

/// `--settings <document>` followed by the caller's arguments, unchanged and in
/// order.
pub(crate) fn claude_args(settings: &str, forwarded: &[String]) -> Vec<String> {
    let mut args = vec!["--settings".to_string(), settings.to_string()];
    args.extend(forwarded.iter().cloned());
    args
}

/// Replace this process with `claude`, so it owns the terminal and receives
/// signals directly instead of through a shunt parent that would have to
/// forward them. `exec` only returns on failure.
#[cfg(unix)]
fn exec_claude(settings: &str, forwarded: &[String]) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let error = Command::new(CLAUDE_BIN)
        .args(claude_args(settings, forwarded))
        .exec();
    Err(describe_launch_failure(error))
}

/// No `exec` off Unix: spawn instead and exit with the child's own status, so a
/// caller scripting `shunt gateway claude` still sees Claude Code's exit code.
#[cfg(not(unix))]
fn exec_claude(settings: &str, forwarded: &[String]) -> anyhow::Result<()> {
    let status = Command::new(CLAUDE_BIN)
        .args(claude_args(settings, forwarded))
        .status()
        .map_err(describe_launch_failure)?;
    std::process::exit(status.code().unwrap_or(1));
}

fn describe_launch_failure(error: io::Error) -> anyhow::Error {
    if error.kind() == io::ErrorKind::NotFound {
        return anyhow!(
            "`{CLAUDE_BIN}` was not found on PATH; install Claude Code, or add it to PATH, then \
             re-run `shunt gateway claude`"
        );
    }
    anyhow::Error::new(error).context(format!("failed to run `{CLAUDE_BIN}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    #[test]
    fn settings_point_the_client_at_the_gateway_without_touching_gateway_mode() {
        let document =
            settings_document("https://gateway.example", Path::new("/usr/local/bin/shunt"))
                .unwrap();
        let value: Value = serde_json::from_str(&document).unwrap();

        assert_eq!(
            value["env"]["ANTHROPIC_BASE_URL"],
            "https://gateway.example"
        );
        assert_eq!(
            value["apiKeyHelper"],
            "'/usr/local/bin/shunt' gateway token"
        );
        // Neither of these may appear: together they fill Claude Code's
        // `gatewayAuth` slot, which drops the client from 12 negotiated betas
        // to 7 and pins `opus`/`sonnet` to the `-4` models.
        assert!(value["env"].get("CLAUDE_CODE_USE_GATEWAY").is_none());
        assert!(value["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
        assert!(!document.contains("CLAUDE_CODE_USE_GATEWAY"));
        assert!(!document.contains("ANTHROPIC_AUTH_TOKEN"));
    }

    #[cfg(not(windows))]
    #[test]
    fn helper_path_survives_spaces_and_quotes() {
        let document = settings_document(
            "https://gateway.example",
            &PathBuf::from("/Applications/My Tools/shunt"),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&document).unwrap();
        assert_eq!(
            value["apiKeyHelper"], "'/Applications/My Tools/shunt' gateway token",
            "an unquoted path with a space would run `/Applications/My` with `Tools/shunt` as an \
             argument"
        );

        // A single quote in the path must not end the quoted run and let the
        // rest of the path be reinterpreted by the shell.
        assert_eq!(
            shell_quote("/home/o'brien/bin/shunt"),
            r"'/home/o'\''brien/bin/shunt'"
        );
    }

    #[test]
    fn settings_document_is_valid_json_for_a_hostile_gateway_url() {
        // `format!`-built JSON would break here; `serde_json` escapes it.
        let document =
            settings_document("https://gateway.example/\"x\\y", Path::new("/bin/shunt")).unwrap();
        let value: Value = serde_json::from_str(&document).expect("must stay parseable");
        assert_eq!(
            value["env"]["ANTHROPIC_BASE_URL"],
            "https://gateway.example/\"x\\y"
        );
    }

    #[test]
    fn forwarded_arguments_follow_the_settings_flag_in_order() {
        let args = claude_args(
            "{}",
            &[
                "-p".to_string(),
                "hi".to_string(),
                "--model".to_string(),
                "opus".to_string(),
            ],
        );
        assert_eq!(
            args,
            ["--settings", "{}", "-p", "hi", "--model", "opus"],
            "shunt must not reorder, drop, or reinterpret the forwarded arguments"
        );
        assert_eq!(claude_args("{}", &[]), ["--settings", "{}"]);
    }

    #[test]
    fn a_missing_claude_binary_is_reported_plainly() {
        let message = describe_launch_failure(io::Error::from(io::ErrorKind::NotFound)).to_string();
        assert!(message.contains("not found on PATH"), "got: {message}");
        assert!(
            !message.contains("os error"),
            "a raw ENOENT must not reach the operator: {message}"
        );
        // A different failure keeps its own cause rather than being reported as
        // a missing binary.
        let other =
            describe_launch_failure(io::Error::from(io::ErrorKind::PermissionDenied)).to_string();
        assert!(other.contains("failed to run"), "got: {other}");
        assert!(!other.contains("not found on PATH"), "got: {other}");
    }
}
