use std::process::{Command, Output};

fn shunt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shunt"))
        .args(args)
        .output()
        .expect("shunt binary should run")
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("stdout should be UTF-8")
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("stderr should be UTF-8")
}

#[test]
fn bare_add_lists_both_blueprint_kinds() {
    let output = shunt(&["add"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("upstream —"));
    assert!(stdout(&output).contains("provider —"));
    assert!(stderr(&output).is_empty());
}

#[test]
fn named_upstream_prints_blueprint_markdown() {
    let output = shunt(&["add", "upstream", "kimi"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output),
        include_str!("../blueprints/upstream/kimi.md")
    );
}

#[test]
fn print_flag_does_not_change_stdout() {
    let implicit = shunt(&["add", "upstream", "kimi"]);
    let explicit = shunt(&["add", "upstream", "kimi", "--print"]);

    assert!(implicit.status.success(), "stderr: {}", stderr(&implicit));
    assert!(explicit.status.success(), "stderr: {}", stderr(&explicit));
    assert_eq!(implicit.stdout, explicit.stdout);
}

#[test]
fn unknown_name_fails_on_stderr_with_clean_stdout() {
    let output = shunt(&["add", "upstream", "unknown"]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("unknown upstream blueprint"));
}

#[test]
fn invalid_urls_fail_with_specific_error_and_clean_stdout() {
    for (url, reason) in [
        (
            "https://user@example.com/docs",
            "credentials are not allowed",
        ),
        (
            "https://user:secret@example.com/docs",
            "credentials are not allowed",
        ),
        ("https://example.com:bad", "invalid port number"),
    ] {
        let output = shunt(&["add", "provider", url, "--print"]);

        assert!(!output.status.success(), "accepted {url:?}");
        assert!(output.stdout.is_empty(), "stdout leaked {url:?}");
        assert!(stderr(&output).contains("invalid research URL"));
        assert!(
            stderr(&output).contains(reason),
            "missing {reason:?} in {:?}",
            stderr(&output)
        );
    }
}

#[test]
fn non_tty_stdout_and_stderr_have_no_interactive_hint() {
    let output = shunt(&["add", "upstream", "kimi"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(!stdout(&output).contains("Hint:"));
    assert!(stderr(&output).is_empty());
}
