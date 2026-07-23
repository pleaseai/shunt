//! `shunt dashboard setup` — one command to stand up the admin usage dashboard.
//!
//! Standing up the dashboard by hand takes three fiddly steps: add
//! `[server.admin]` (and usually `[server.oauth_usage]`) to the config, invent
//! an admin token and export it as `SHUNT_ADMIN_TOKENS`, then restart. This
//! command collapses that to one invocation: it generates a token into an
//! owner-only file, records that file as `[server.admin].tokens_file` (so no
//! secret has to live in the launch env), enables both dashboard tables, and
//! prints the URL. It is idempotent — re-running reuses the existing token and
//! never duplicates a config block.

use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;

use crate::config::{self, Config, ConfigFormat};

/// What [`setup`] did, for the CLI to report to the user.
pub struct SetupOutcome {
    pub config_path: PathBuf,
    pub token_file: PathBuf,
    /// The token value the user pastes into the browser login (no `name:`
    /// prefix). `None` when `[server.admin]` was already configured, since we
    /// then leave the user's own token source untouched and cannot know it.
    pub token: Option<String>,
    pub token_reused: bool,
    pub admin_block_added: bool,
    pub oauth_usage_block_added: bool,
    /// True when `[server.admin]` was already present and left untouched.
    pub admin_already_configured: bool,
    pub dashboard_url: String,
}

/// Result of the pure config-text transform, separated from IO for testing.
struct EnsureResult {
    text: String,
    admin_block_added: bool,
    oauth_usage_block_added: bool,
    admin_already_configured: bool,
}

/// Run the setup: resolve the config file, ensure the dashboard blocks exist,
/// generate/reuse the admin token file, and compute the dashboard URL.
pub fn setup(explicit_config: Option<&Path>) -> anyhow::Result<SetupOutcome> {
    let config_path = match explicit_config {
        Some(path) => path.to_path_buf(),
        None => Config::find_config_file().unwrap_or_else(default_new_config_path),
    };

    let token_file = config::default_admin_token_file()
        .context("cannot determine ~/.shunt/admin-token: neither HOME nor USERPROFILE is set")?;

    let existing = match std::fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", config_path.display()))
        }
    };

    // Only mint/wire our own token file when we are the ones adding
    // `[server.admin]`. If the user already configured it (env var or their own
    // file), we must not overwrite their token source or print a token that
    // will not work.
    let admin_present = has_uncommented_table(&existing, "server.admin");
    let (token, token_reused) = if admin_present {
        (None, false)
    } else {
        // Reuse an existing file so re-running never invalidates a token a
        // running gateway already trusts; otherwise mint a fresh one.
        match read_existing_token(&token_file) {
            Some(existing_token) => (Some(existing_token), true),
            None => {
                let minted = mint_token();
                crate::atomic_file::write_private_atomic(
                    &token_file,
                    format!("admin:{minted}\n").as_bytes(),
                )
                .with_context(|| format!("writing admin token file {}", token_file.display()))?;
                (Some(minted), false)
            }
        }
    };

    // Config: ensure the two dashboard blocks exist without disturbing comments
    // or key order in the rest of the file.
    let ensured = ensure_blocks(&existing, &token_file, &config_path);
    if ensured.text != existing {
        crate::atomic_file::write_private_atomic(&config_path, ensured.text.as_bytes())
            .with_context(|| format!("writing {}", config_path.display()))?;
    }

    Ok(SetupOutcome {
        config_path: config_path.clone(),
        token_file,
        token,
        token_reused,
        admin_block_added: ensured.admin_block_added,
        oauth_usage_block_added: ensured.oauth_usage_block_added,
        admin_already_configured: ensured.admin_already_configured,
        dashboard_url: dashboard_url(&existing, &config_path),
    })
}

/// Ensure `[server.admin]` (with a `tokens_file`) and `[server.oauth_usage]` are
/// present. Pure text transform for TOML: appends only the blocks that are
/// missing and never touches an existing `[server.admin]`. For YAML, parses and
/// merges `server.admin` / `server.oauth_usage` so the file stays valid YAML.
fn ensure_blocks(existing: &str, token_file: &Path, config_path: &Path) -> EnsureResult {
    match ConfigFormat::from_path(config_path) {
        ConfigFormat::Toml => ensure_blocks_toml(existing, token_file),
        ConfigFormat::Yaml => ensure_blocks_yaml(existing, token_file),
    }
}

fn ensure_blocks_toml(existing: &str, token_file: &Path) -> EnsureResult {
    let admin_present = has_uncommented_table(existing, "server.admin");
    let oauth_present = has_uncommented_table(existing, "server.oauth_usage");

    let mut additions = String::new();
    if !admin_present {
        additions.push_str(&format!(
            "\n# ── Usage dashboard (added by `shunt dashboard setup`) ──────────────────\n\
             [server.admin]\n\
             tokens_file = \"{}\"\n",
            format_config_path(token_file)
        ));
    }
    if !oauth_present {
        additions.push_str(
            "\n# Serve GET /api/oauth/usage so Claude Code's native /usage bars render.\n\
             [server.oauth_usage]\n",
        );
    }

    let mut text = existing.to_string();
    if !additions.is_empty() {
        // Guarantee exactly one newline between prior content and our blocks.
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&additions);
    }

    EnsureResult {
        text,
        admin_block_added: !admin_present,
        oauth_usage_block_added: !oauth_present,
        admin_already_configured: admin_present,
    }
}

fn ensure_blocks_yaml(existing: &str, token_file: &Path) -> EnsureResult {
    let admin_present = yaml_has_table(existing, "server", "admin");
    let oauth_present = yaml_has_table(existing, "server", "oauth_usage");

    let mut root = if existing.trim().is_empty() {
        serde_yaml::Mapping::new()
    } else {
        match serde_yaml::from_str::<serde_yaml::Value>(existing) {
            Ok(serde_yaml::Value::Mapping(mapping)) => mapping,
            Ok(_) => {
                return EnsureResult {
                    text: existing.to_string(),
                    admin_block_added: false,
                    oauth_usage_block_added: false,
                    admin_already_configured: admin_present,
                };
            }
            Err(_) => {
                return EnsureResult {
                    text: existing.to_string(),
                    admin_block_added: false,
                    oauth_usage_block_added: false,
                    admin_already_configured: admin_present,
                };
            }
        }
    };

    let mut changed = false;
    if !admin_present {
        let server = root
            .entry(serde_yaml::Value::String("server".to_string()))
            .or_insert_with(|| serde_yaml::Mapping::new().into());
        if let Some(server) = server.as_mapping_mut() {
            let admin = server
                .entry(serde_yaml::Value::String("admin".to_string()))
                .or_insert_with(|| serde_yaml::Mapping::new().into());
            if let Some(admin) = admin.as_mapping_mut() {
                admin.insert(
                    serde_yaml::Value::String("tokens_file".to_string()),
                    serde_yaml::Value::String(format_config_path(token_file)),
                );
                changed = true;
            }
        }
    }
    if !oauth_present {
        let server = root
            .entry(serde_yaml::Value::String("server".to_string()))
            .or_insert_with(|| serde_yaml::Mapping::new().into());
        if let Some(server) = server.as_mapping_mut() {
            server.insert(
                serde_yaml::Value::String("oauth_usage".to_string()),
                serde_yaml::Mapping::new().into(),
            );
            changed = true;
        }
    }

    let text = if changed {
        serde_yaml::to_string(&serde_yaml::Value::Mapping(root))
            .unwrap_or_else(|_| existing.to_string())
    } else {
        existing.to_string()
    };

    EnsureResult {
        text,
        admin_block_added: !admin_present && changed,
        oauth_usage_block_added: !oauth_present && changed,
        admin_already_configured: admin_present,
    }
}

fn yaml_has_table(existing: &str, section: &str, table: &str) -> bool {
    if existing.trim().is_empty() {
        return false;
    }
    let Ok(serde_yaml::Value::Mapping(root)) = serde_yaml::from_str(existing) else {
        return false;
    };
    root.get(section)
        .and_then(|section| section.get(table))
        .is_some()
}

/// Format a filesystem path for insertion into TOML/YAML config text. Forward
/// slashes avoid invalid TOML escape sequences on Windows backslash paths.
fn format_config_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// True when an uncommented table header for `dotted` (its own table or a
/// subtable) appears in the TOML text. Ignores commented (`# [...]`) lines.
fn has_uncommented_table(text: &str, dotted: &str) -> bool {
    let own = format!("[{dotted}]");
    let sub = format!("[{dotted}.");
    text.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            return false;
        }
        trimmed == own || trimmed.starts_with(&sub)
    })
}

/// The dashboard URL, using the configured bind when the file parses, else the
/// documented default. A wildcard bind is shown as loopback since that is where
/// a local browser reaches it.
fn dashboard_url(existing: &str, config_path: &Path) -> String {
    let bind = match ConfigFormat::from_path(config_path) {
        ConfigFormat::Yaml => serde_yaml::from_str::<serde_yaml::Value>(existing)
            .ok()
            .and_then(|value| {
                value
                    .get("server")?
                    .get("bind")?
                    .as_str()
                    .map(str::to_string)
            }),
        ConfigFormat::Toml => toml::from_str::<toml::Value>(existing)
            .ok()
            .and_then(|value| {
                value
                    .get("server")?
                    .get("bind")?
                    .as_str()
                    .map(str::to_string)
            }),
    }
    .unwrap_or_else(|| "127.0.0.1:3001".to_string());
    let host_port = bind
        .replace("0.0.0.0", "127.0.0.1")
        .replace("[::]", "127.0.0.1");
    format!("http://{host_port}/admin")
}

/// Read and validate the token from an existing token file; `None` if absent,
/// unreadable, or malformed (so the caller mints a fresh one).
fn read_existing_token(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let normalised = contents.replace(['\r', '\n'], ",");
    let pairs = crate::auth::inbound::parse_tokens(&normalised).ok()?;
    pairs.into_iter().next().map(|(_, token)| token)
}

/// 32 random bytes, URL-safe base64 (no padding) — a 43-char opaque admin
/// token, mirroring the PKCE-secret helper in `auth::shared`.
fn mint_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `~/.config/shunt/shunt.toml` — where a brand-new config is created when none
/// is found. Falls back to the working directory when HOME is unset.
fn default_new_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        });
    match base {
        Some(dir) => dir.join("shunt").join("shunt.toml"),
        None => PathBuf::from("shunt.toml"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_both_blocks_to_empty_config() {
        let out = ensure_blocks(
            "",
            Path::new("/home/u/.shunt/admin-token"),
            Path::new("shunt.toml"),
        );
        assert!(out.admin_block_added);
        assert!(out.oauth_usage_block_added);
        assert!(!out.admin_already_configured);
        assert!(out.text.contains("[server.admin]"));
        assert!(out
            .text
            .contains("tokens_file = \"/home/u/.shunt/admin-token\""));
        assert!(out.text.contains("[server.oauth_usage]"));
    }

    #[test]
    fn is_idempotent_when_both_blocks_present() {
        let src = "[server.admin]\ntokens_file = \"/x\"\n\n[server.oauth_usage]\n";
        let out = ensure_blocks(
            src,
            Path::new("/home/u/.shunt/admin-token"),
            Path::new("shunt.toml"),
        );
        assert!(!out.admin_block_added);
        assert!(!out.oauth_usage_block_added);
        assert!(out.admin_already_configured);
        assert_eq!(out.text, src, "no changes when already configured");
    }

    #[test]
    fn respects_existing_admin_but_adds_oauth_usage() {
        let src = "[server.admin]\ntokens_env = \"MY_TOKENS\"\n";
        let out = ensure_blocks(src, Path::new("/t"), Path::new("shunt.toml"));
        assert!(
            !out.admin_block_added,
            "must not touch an existing admin block"
        );
        assert!(out.oauth_usage_block_added);
        assert!(out.text.contains("tokens_env = \"MY_TOKENS\""));
        assert!(out.text.contains("[server.oauth_usage]"));
        // The user's own admin block keeps its env-based tokens.
        assert!(!out.text.contains("tokens_file"));
    }

    #[test]
    fn ignores_commented_out_blocks() {
        let src = "# [server.admin]\n# [server.oauth_usage]\n";
        let out = ensure_blocks(src, Path::new("/t"), Path::new("shunt.toml"));
        assert!(out.admin_block_added);
        assert!(out.oauth_usage_block_added);
    }

    #[test]
    fn treats_admin_subtable_as_present() {
        let src = "[server.admin.oidc]\npublic_url = \"https://x\"\n";
        let out = ensure_blocks(src, Path::new("/t"), Path::new("shunt.toml"));
        assert!(
            !out.admin_block_added,
            "an oidc subtable implies admin is configured"
        );
    }

    #[test]
    fn dashboard_url_uses_configured_bind() {
        let url = dashboard_url(
            "[server]\nbind = \"127.0.0.1:9000\"\n",
            Path::new("shunt.toml"),
        );
        assert_eq!(url, "http://127.0.0.1:9000/admin");
    }

    #[test]
    fn dashboard_url_maps_wildcard_to_loopback() {
        let url = dashboard_url(
            "[server]\nbind = \"0.0.0.0:3001\"\n",
            Path::new("shunt.toml"),
        );
        assert_eq!(url, "http://127.0.0.1:3001/admin");
    }

    #[test]
    fn dashboard_url_falls_back_on_missing_bind() {
        assert_eq!(
            dashboard_url("", Path::new("shunt.toml")),
            "http://127.0.0.1:3001/admin"
        );
    }

    #[test]
    fn dashboard_url_reads_bind_from_yaml_config() {
        let url = dashboard_url("server:\n  bind: 127.0.0.1:9000\n", Path::new("shunt.yaml"));
        assert_eq!(url, "http://127.0.0.1:9000/admin");
    }

    #[test]
    fn minted_tokens_are_unique_and_url_safe() {
        let a = mint_token();
        let b = mint_token();
        assert_eq!(a.len(), 43, "32 bytes base64-no-pad");
        assert_ne!(a, b);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn toml_path_uses_forward_slashes_on_windows_paths() {
        let out = ensure_blocks(
            "",
            Path::new(r"C:\Users\u\.shunt\admin-token"),
            Path::new("shunt.toml"),
        );
        assert!(out
            .text
            .contains(r#"tokens_file = "C:/Users/u/.shunt/admin-token""#));
    }

    #[test]
    fn yaml_setup_emits_valid_yaml_blocks() {
        let out = ensure_blocks(
            "server:\n  bind: 127.0.0.1:3001\n",
            Path::new("/home/u/.shunt/admin-token"),
            Path::new("shunt.yaml"),
        );
        assert!(out.admin_block_added);
        assert!(out.oauth_usage_block_added);
        let parsed: serde_yaml::Value = serde_yaml::from_str(&out.text).expect("valid yaml");
        assert_eq!(
            parsed["server"]["admin"]["tokens_file"].as_str(),
            Some("/home/u/.shunt/admin-token")
        );
        assert!(parsed["server"]["oauth_usage"].is_mapping());
        assert!(!out.text.contains("[server.admin]"));
    }
}
