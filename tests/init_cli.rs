use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "shunt-init-cli-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shunt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shunt"))
        .args(args)
        .output()
        .expect("shunt binary should run")
}

fn init(root: &Path, extra: &[&str]) -> Output {
    let mut args = vec!["init", "--root", root.to_str().expect("UTF-8 temp path")];
    args.extend_from_slice(extra);
    shunt(&args)
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("stdout should be UTF-8")
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("stderr should be UTF-8")
}

#[test]
fn init_creates_starter_and_refuses_to_replace_it_without_force() {
    let dir = TempDir::new("create");
    let path = dir.0.join("shunt.toml");

    let first = init(&dir.0, &[]);
    assert!(first.status.success(), "stderr: {}", stderr(&first));
    assert_eq!(stdout(&first), format!("Wrote {}\n", path.display()));
    assert!(stderr(&first).is_empty());
    let original = std::fs::read_to_string(&path).expect("starter should exist");

    let second = init(&dir.0, &[]);
    assert!(!second.status.success());
    assert!(second.stdout.is_empty());
    assert!(stderr(&second).contains("shunt.toml"));
    assert!(stderr(&second).contains("--force"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn yaml_variant_blocks_init_but_survives_forced_toml_creation() {
    let dir = TempDir::new("yaml");
    let yaml = dir.0.join("shunt.yaml");
    std::fs::write(&yaml, "server:\n  bind: 127.0.0.1:3001\n").unwrap();

    let blocked = init(&dir.0, &[]);
    assert!(!blocked.status.success());
    assert!(blocked.stdout.is_empty());
    assert!(stderr(&blocked).contains("shunt.yaml"));
    assert!(!dir.0.join("shunt.toml").exists());

    let forced = init(&dir.0, &["--force"]);
    assert!(forced.status.success(), "stderr: {}", stderr(&forced));
    assert!(dir.0.join("shunt.toml").exists());
    assert_eq!(
        std::fs::read_to_string(yaml).unwrap(),
        "server:\n  bind: 127.0.0.1:3001\n"
    );
}

#[test]
fn missing_root_directory_is_rejected_without_creation() {
    let parent = TempDir::new("missing");
    let missing = parent.0.join("does-not-exist");

    let output = init(&missing, &[]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr(&output).contains("root directory does not exist"));
    assert!(!missing.exists());
}

#[test]
fn unknown_and_duplicate_upstreams_are_rejected() {
    let unknown_dir = TempDir::new("unknown");
    let unknown = init(&unknown_dir.0, &["--upstream", "unknown"]);
    assert!(!unknown.status.success());
    assert!(unknown.stdout.is_empty());
    let error = stderr(&unknown);
    assert!(error.contains("unknown upstream preset"));
    for name in [
        "anthropic",
        "codex",
        "openai",
        "xai",
        "grok",
        "kimi",
        "cursor",
    ] {
        assert!(error.contains(name), "missing {name:?} in {error:?}");
    }
    assert!(!unknown_dir.0.join("shunt.toml").exists());

    let duplicate_dir = TempDir::new("duplicate");
    let duplicate = init(
        &duplicate_dir.0,
        &["--upstream", "codex", "--upstream", "codex"],
    );
    assert!(!duplicate.status.success());
    assert!(duplicate.stdout.is_empty());
    assert!(stderr(&duplicate).contains("duplicate upstream preset"));
    assert!(!duplicate_dir.0.join("shunt.toml").exists());
}
