//! All inputs are synthetic; never read the operator's OpenCodex home.
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    output: PathBuf,
}
impl Fixture {
    fn new(config: serde_json::Value, auth: serde_json::Value) -> Self {
        let root = std::env::temp_dir().join(format!("shunt-import-test-{}", uuid::Uuid::new_v4()));
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("config.json"), config.to_string()).unwrap();
        fs::write(source.join("auth.json"), auth.to_string()).unwrap();
        let output = root.join("output");
        Self {
            root,
            source,
            output,
        }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_shunt"))
            .args(["import", "opencodex", "--from"])
            .arg(&self.source)
            .arg("--output-dir")
            .arg(&self.output)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }
    fn files(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.output)
            .unwrap()
            .map(|p| p.unwrap().path().join("credentials.env"))
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn key_config() -> serde_json::Value {
    json!({"providers":{"demo":{"adapter":"openai-chat","apiKey":"fixture-secret"}}})
}
fn transcript(result: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    )
}

#[test]
fn preview_is_read_only_redacted_and_noninteractive_writes_require_confirmation() {
    let f = Fixture::new(key_config(), json!({}));
    let before = fs::read(f.source.join("config.json")).unwrap();
    let mtime = fs::metadata(f.source.join("config.json"))
        .unwrap()
        .modified()
        .unwrap();
    let result = f.run(&["--dry-run"]);
    assert!(result.status.success(), "{}", transcript(&result));
    assert!(transcript(&result).contains("SHUNT_IMPORTED_DEMO_API_KEY"));
    assert!(!transcript(&result).contains("fixture-secret"));
    assert!(!f.output.exists());
    assert!(!f.run(&[]).status.success());
    assert!(!f.output.exists());
    assert_eq!(fs::read(f.source.join("config.json")).unwrap(), before);
    assert_eq!(
        fs::metadata(f.source.join("config.json"))
            .unwrap()
            .modified()
            .unwrap(),
        mtime
    );
    assert_eq!(fs::read_dir(&f.source).unwrap().count(), 2);
}

#[test]
#[cfg(unix)]
fn snapshots_are_private_non_overwriting_and_shell_safe() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(
        json!({"providers":{"demo":{"adapter":"openai-chat","apiKey":"fixture'$(false)`false`"}}}),
        json!({}),
    );
    for _ in 0..2 {
        let out = f.run(&["--yes"]);
        assert!(out.status.success(), "{}", transcript(&out));
    }
    let files = f.files();
    assert_eq!(files.len(), 2);
    for file in files {
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let out = Command::new("sh")
            .args([
                "-c",
                ". \"$1\"; printf '%s' \"$SHUNT_IMPORTED_DEMO_API_KEY\"",
                "fixture",
            ])
            .arg(&file)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"fixture'$(false)`false`");
    }
}

#[test]
fn imports_active_oauth_only_without_refresh_tokens() {
    let f = Fixture::new(
        json!({}),
        json!({"cursor":{"activeAccountId":"chosen","accounts":[
        {"id":"other","credential":{"access":"other-access","expires":9007199254740991u64}},
        {"id":"chosen","credential":{"access":"fixture-access","refresh":"never-export-this","expires":9007199254740991u64}}
    ]},"command-code":{"access":"fixture-command","refresh":"never-export-command","expires":9007199254740991u64}}),
    );
    let out = f.run(&["--yes", "--provider", "cursor"]);
    assert!(out.status.success(), "{}", transcript(&out));
    let env = fs::read_to_string(&f.files()[0]).unwrap();
    assert!(env.contains("SHUNT_CURSOR_AUTH_TOKEN='fixture-access'"));
    assert!(!env.contains("never-export"));
    assert!(!env.contains("other-access"));
    assert!(!env.contains("fixture-command"));
    assert!(!transcript(&out).contains("fixture-access"));
    let out = f.run(&["--yes", "--provider", "command-code"]);
    assert!(out.status.success());
    assert!(f.files().iter().any(|p| fs::read_to_string(p)
        .unwrap()
        .contains("SHUNT_COMMANDCODE_API_KEY='fixture-command'")));
}

#[test]
fn refuses_expired_ambiguous_unsupported_and_invalid_sources() {
    for auth in [
        json!({"cursor":{"access":"fixture","expires":1}}),
        json!({"cursor":{"activeAccountId":"a","accounts":[{"id":"a","needsReauth":true,"credential":{"access":"fixture","expires":9007199254740991u64}}]}}),
        json!({"cursor":{"activeAccountId":"a","accounts":[{"id":"a"},{"id":"a"}]}}),
        json!({"google-antigravity":{"access":"fixture","expires":9007199254740991u64}}),
        json!({"cursor":{"access":"bad\nvalue","expires":9007199254740991u64}}),
    ] {
        let f = Fixture::new(json!({}), auth);
        assert!(!f.run(&["--yes"]).status.success());
        assert!(!f.output.exists());
    }
    let f = Fixture::new(key_config(), json!({}));
    assert!(!f.run(&["--provider", "missing", "--yes"]).status.success());
    fs::write(f.source.join("auth.json"), b"{\"secret-fragment").unwrap();
    let out = f.run(&["--yes"]);
    assert!(!out.status.success());
    assert!(!transcript(&out).contains("secret-fragment"));
    assert!(!f.output.exists());
}

#[test]
fn refuses_output_inside_source_and_name_collisions() {
    let f = Fixture::new(key_config(), json!({}));
    let out = Command::new(env!("CARGO_BIN_EXE_shunt"))
        .args(["import", "opencodex", "--yes", "--from"])
        .arg(&f.source)
        .arg("--output-dir")
        .arg(f.source.join("new"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!f.source.join("new").exists());
    let f = Fixture::new(
        json!({"providers":{"a-b":{"adapter":"anthropic","apiKey":"one"},"a_b":{"adapter":"anthropic","apiKey":"two"}}}),
        json!({}),
    );
    assert!(!f.run(&["--yes"]).status.success());
    assert!(!f.output.exists());
}

#[test]
#[cfg(unix)]
fn refuses_symlink_sources_and_oversized_files() {
    let f = Fixture::new(key_config(), json!({}));
    fs::rename(f.source.join("auth.json"), f.root.join("auth.json")).unwrap();
    std::os::unix::fs::symlink(f.root.join("auth.json"), f.source.join("auth.json")).unwrap();
    assert!(!f.run(&["--yes"]).status.success());
    fs::remove_file(f.source.join("auth.json")).unwrap();
    fs::write(f.source.join("auth.json"), vec![b' '; 4 * 1024 * 1024 + 1]).unwrap();
    assert!(!f.run(&["--yes"]).status.success());
    assert!(!f.output.exists());
}

#[test]
fn respects_environment_source_and_ignores_gateway_keys_and_inactive_pools() {
    let f = Fixture::new(
        json!({
            "apiKeys":[{"key":"gateway-secret"}],
            "providers":{
                "pooled":{"adapter":"openai-chat","apiKey":"active-secret","apiKeyPool":[{"key":"inactive-secret"}]},
                "disabled":{"adapter":"openai-chat","disabled":true,"apiKey":"disabled-secret"},
                "forward":{"adapter":"forward","apiKey":"forward-secret"}
            }
        }),
        json!({}),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_shunt"))
        .args(["import", "opencodex", "--yes", "--output-dir"])
        .arg(&f.output)
        .env("OPENCODEX_HOME", &f.source)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", transcript(&out));
    let env = fs::read_to_string(&f.files()[0]).unwrap();
    assert!(env.contains("active-secret"));
    for excluded in [
        "gateway-secret",
        "inactive-secret",
        "disabled-secret",
        "forward-secret",
    ] {
        assert!(!env.contains(excluded));
        assert!(!transcript(&out).contains(excluded));
    }
}

#[test]
fn empty_environment_source_falls_through_to_default_home() {
    let f = Fixture::new(key_config(), json!({}));
    let isolated_home = f.root.join("isolated_home");
    let ocx = isolated_home.join(".opencodex");
    fs::create_dir_all(&ocx).unwrap();
    fs::write(ocx.join("config.json"), key_config().to_string()).unwrap();
    fs::write(ocx.join("auth.json"), "{}").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_shunt"))
        .args(["import", "opencodex", "--yes", "--output-dir"])
        .arg(&f.output)
        .env("OPENCODEX_HOME", "")
        .env("HOME", &isolated_home)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", transcript(&out));
    let env = fs::read_to_string(&f.files()[0]).unwrap();
    assert!(env.contains("SHUNT_IMPORTED_DEMO_API_KEY"));
}
