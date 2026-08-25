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

use std::{ffi::OsString, io, path::Path, process::Command};

use anyhow::{anyhow, Context};
use serde_json::json;

use super::store;

const CLAUDE_BIN: &str = "claude";

/// Ambient variables removed from the launched process.
///
/// Mirrors the enumerations in the shipped **Claude Code 2.1.234** binary —
/// re-check this list against the binary on every upgrade, because a name added
/// there and not here silently reopens the channel.
///
/// Why this is needed at all: the settings document's `env` block names only
/// `ANTHROPIC_BASE_URL`, and 2.1.234 applies a settings `env` block as
/// `Object.assign(process.env, …)`, so every variable the block does *not* name
/// survives into the child. Two measured consequences:
///
/// * `ANTHROPIC_AUTH_TOKEN` beats `apiKeyHelper` unconditionally — the helper
///   is consulted only when the ambient token is absent.
/// * `CLAUDE_CODE_USE_GATEWAY` together with a base URL and an auth token fills
///   the gateway credential slot, flipping the client into "gateway" provider
///   mode, a branch that never consults `apiKeyHelper` at all.
///
/// So the invoking shell could otherwise defeat this subcommand's whole point.
///
/// Deliberately **not** removed:
///
/// * `ANTHROPIC_BASE_URL` — the settings document injects it, and its `env`
///   block already beats an ambient value (see this module's doc comment).
/// * the other `*_BASE_URL` variables, and the `AWS_*` / `GOOGLE_*`
///   credential-file variables — each is read only under a provider mode whose
///   selector is stripped above, so removing them would add names without
///   closing a reachable path. That is a reasoned omission, not an oversight.
///
/// HONESTY: this closes the **ambient-environment** channel. It does not
/// guarantee first-party mode, and the comment must not be reworded to say it
/// does. Verified channels this cannot reach: a settings-file `env` block
/// re-injects these *after* launch (`applyConfigEnvironmentVariables` assigns
/// from every settings source), `apiKeyHelper` is itself a settings key a user
/// may already have set, an existing saved login lives in the credential store,
/// and both file-descriptor readers fall back to a `wellKnownPath` consulted
/// with no variable set at all.
const SCRUBBED_ENV: &[&str] = &[
    // Credentials.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "ANTHROPIC_AWS_API_KEY",
    // Provider-mode selectors.
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_GATEWAY",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_AWS_WORKSPACE_ID",
    "ANTHROPIC_GOOGLE_CLOUD_PROJECT",
    "ANTHROPIC_GOOGLE_CLOUD_LOCATION",
    "ANTHROPIC_GOOGLE_CLOUD_WORKSPACE_ID",
    "CLOUD_ML_REGION",
    // Skip-auth switches.
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
    "CLAUDE_CODE_SKIP_ANTHROPIC_AWS_AUTH",
    "CLAUDE_CODE_SKIP_ANTHROPIC_GOOGLE_CLOUD_AUTH",
    "CLAUDE_CODE_SKIP_MANTLE_AUTH",
    // Host indirection.
    "ANTHROPIC_UNIX_SOCKET",
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
    HOST_AUTH_ENV_VAR,
    "CLAUDE_CODE_HOST_CREDS_FILE",
    // Header injection.
    "ANTHROPIC_CUSTOM_HEADERS",
    // Indirect credential readers.
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
];

/// The one entry a fixed list cannot express: this variable holds the **name**
/// of another variable that carries the credential, so whatever it points at
/// has to be removed too or the deny-list is provably incomplete.
const HOST_AUTH_ENV_VAR: &str = "CLAUDE_CODE_HOST_AUTH_ENV_VAR";

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
    // Fail here, not lossily: a `to_string_lossy` replacement character would
    // build a helper command naming a file that does not exist, and Claude Code
    // would surface that as an opaque auth error at first use — the exact
    // deferred failure this module's quoting exists to prevent.
    let executable = shunt_executable.to_str().ok_or_else(|| {
        anyhow!(
            "the shunt executable path {} is not valid UTF-8, so it cannot be embedded in the \
             Claude Code apiKeyHelper command; reinstall shunt under a UTF-8 path, or set \
             `apiKeyHelper` yourself in your Claude Code settings",
            shunt_executable.display()
        )
    })?;
    let helper = format!("{} gateway token", shell_quote(executable));
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

/// Every variable to strip from the child, resolved against `lookup` — the
/// process environment in production.
///
/// Split from the [`Command`] so the resolution, including the
/// [`HOST_AUTH_ENV_VAR`] indirection, can be exercised without mutating the
/// test binary's own environment.
fn scrubbed_env_names(lookup: impl Fn(&str) -> Option<OsString>) -> Vec<OsString> {
    // Read the pointer *before* anything is removed: its value names the
    // variable actually carrying the credential, and that name is only
    // knowable at runtime.
    let indirect = lookup(HOST_AUTH_ENV_VAR).filter(|name| !name.is_empty());
    SCRUBBED_ENV
        .iter()
        .map(OsString::from)
        .chain(indirect)
        .collect()
}

/// The child process both [`exec_claude`] arms launch.
///
/// One builder rather than two, deliberately: a scrub applied in only one arm
/// is worse than none, because it reads as done. Anything that must hold for
/// the launched client belongs here.
fn claude_command(settings: &str, forwarded: &[String]) -> Command {
    let mut command = Command::new(CLAUDE_BIN);
    command.args(claude_args(settings, forwarded));
    for name in scrubbed_env_names(|name| std::env::var_os(name)) {
        command.env_remove(name);
    }
    command
}

/// Replace this process with `claude`, so it owns the terminal and receives
/// signals directly instead of through a shunt parent that would have to
/// forward them. `exec` only returns on failure.
#[cfg(unix)]
fn exec_claude(settings: &str, forwarded: &[String]) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let error = claude_command(settings, forwarded).exec();
    Err(describe_launch_failure(error))
}

/// No `exec` off Unix: spawn instead and exit with the child's own status, so a
/// caller scripting `shunt gateway claude` still sees Claude Code's exit code.
#[cfg(not(unix))]
fn exec_claude(settings: &str, forwarded: &[String]) -> anyhow::Result<()> {
    let status = claude_command(settings, forwarded)
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
    use std::{ffi::OsStr, path::PathBuf};

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

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_executable_path_fails_instead_of_being_mangled() {
        use std::os::unix::ffi::OsStrExt;

        // `to_string_lossy` would turn this into a U+FFFD and produce a helper
        // command naming a file that does not exist, which Claude Code reports
        // only later and only as an auth failure.
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"/opt/sh\xffunt"));
        let error = settings_document("https://gateway.example", &path)
            .expect_err("a path that cannot round-trip must not be embedded");
        let message = error.to_string();
        assert!(message.contains("not valid UTF-8"), "got: {message}");
        assert!(
            message.contains("apiKeyHelper"),
            "the message must name what breaks: {message}"
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

    /// What the child would actually receive: `env_remove` shows up in
    /// `get_envs` as a name with no value, so this inspects the real
    /// [`Command`] both `exec_claude` arms launch rather than the constant.
    fn removed_by_the_launcher() -> Vec<OsString> {
        claude_command("{}", &[])
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_os_string())
            .collect()
    }

    /// A settings `env` block is applied as `Object.assign(process.env, …)`, so
    /// every ambient variable it does not name survives — and an ambient
    /// `ANTHROPIC_AUTH_TOKEN` beats `apiKeyHelper` outright, while
    /// `CLAUDE_CODE_USE_GATEWAY` flips the client to a provider mode that never
    /// consults the helper at all.
    ///
    /// Enumerated here independently of `SCRUBBED_ENV`: dropping a name from
    /// the production list fails this test rather than silently shrinking the
    /// deny-list along with it.
    #[test]
    fn every_ambient_credential_and_provider_mode_channel_is_stripped() {
        let removed = removed_by_the_launcher();
        for name in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "AWS_BEARER_TOKEN_BEDROCK",
            "ANTHROPIC_FOUNDRY_API_KEY",
            "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
            "ANTHROPIC_AWS_API_KEY",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
            "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
            "CLAUDE_CODE_USE_MANTLE",
            "CLAUDE_CODE_USE_GATEWAY",
            "ANTHROPIC_FOUNDRY_RESOURCE",
            "ANTHROPIC_VERTEX_PROJECT_ID",
            "ANTHROPIC_AWS_WORKSPACE_ID",
            "ANTHROPIC_GOOGLE_CLOUD_PROJECT",
            "ANTHROPIC_GOOGLE_CLOUD_LOCATION",
            "ANTHROPIC_GOOGLE_CLOUD_WORKSPACE_ID",
            "CLOUD_ML_REGION",
            "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
            "CLAUDE_CODE_SKIP_VERTEX_AUTH",
            "CLAUDE_CODE_SKIP_FOUNDRY_AUTH",
            "CLAUDE_CODE_SKIP_ANTHROPIC_AWS_AUTH",
            "CLAUDE_CODE_SKIP_ANTHROPIC_GOOGLE_CLOUD_AUTH",
            "CLAUDE_CODE_SKIP_MANTLE_AUTH",
            "ANTHROPIC_UNIX_SOCKET",
            "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
            "CLAUDE_CODE_HOST_AUTH_ENV_VAR",
            "CLAUDE_CODE_HOST_CREDS_FILE",
            "ANTHROPIC_CUSTOM_HEADERS",
            "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
            "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
        ] {
            assert!(
                removed.iter().any(|removed| removed == OsStr::new(name)),
                "{name} would reach the launched client"
            );
        }

        // The one variable the settings document injects: stripping it would
        // point the client back at api.anthropic.com.
        assert!(
            !removed
                .iter()
                .any(|name| name == OsStr::new("ANTHROPIC_BASE_URL")),
            "ANTHROPIC_BASE_URL is ours to set, not to strip"
        );
    }

    /// The dynamic entry, which no fixed list can express:
    /// `CLAUDE_CODE_HOST_AUTH_ENV_VAR` holds the *name* of the variable that
    /// actually carries the credential. The name below appears nowhere in
    /// `SCRUBBED_ENV`, so it can only be removed by following the pointer.
    #[test]
    fn the_host_auth_pointer_is_followed_to_the_variable_it_names() {
        let names = scrubbed_env_names(|name| {
            (name == HOST_AUTH_ENV_VAR).then(|| OsString::from("MY_COMPANY_CLAUDE_TOKEN"))
        });
        assert!(
            names
                .iter()
                .any(|name| name == OsStr::new("MY_COMPANY_CLAUDE_TOKEN")),
            "the credential the pointer names must be stripped too, or the deny-list is \
             incomplete by construction"
        );
        // The pointer itself still goes, and an unset pointer adds nothing.
        assert!(names
            .iter()
            .any(|name| name == OsStr::new(HOST_AUTH_ENV_VAR)));
        assert_eq!(scrubbed_env_names(|_| None).len(), SCRUBBED_ENV.len());
        // An empty pointer names no variable; removing "" would be a no-op
        // entry that only makes the list look longer than it is.
        assert_eq!(
            scrubbed_env_names(|_| Some(OsString::new())).len(),
            SCRUBBED_ENV.len()
        );
    }

    /// The pointer is resolved against the live environment, not only against
    /// an injected lookup — the wiring `claude_command` depends on.
    #[tokio::test]
    async fn the_launcher_resolves_the_host_auth_pointer_from_the_real_environment() {
        let _guard = store::TEST_ENV_LOCK.lock().await;
        let _pointer =
            crate::auth::shared::EnvVarGuard::set(HOST_AUTH_ENV_VAR, "SHUNT_TEST_HOST_CREDENTIAL");

        assert!(
            removed_by_the_launcher()
                .iter()
                .any(|name| name == OsStr::new("SHUNT_TEST_HOST_CREDENTIAL")),
            "the launched command must strip whatever {HOST_AUTH_ENV_VAR} points at"
        );
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
