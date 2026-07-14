//! `shunt login codex` — import the current `codex login` credential into a
//! named shunt-owned account so it can join the account pool.
//!
//! Unlike Claude, Codex has no shunt-driven OAuth flow (no PKCE loopback, no
//! setup-token concept): the CLI's own `codex login` already produces a
//! refreshable `~/.codex/auth.json`, so `shunt login codex` only needs to copy
//! it into the shunt-owned account store.

use std::path::PathBuf;

use anyhow::Context;

use super::store;

pub async fn run(name: &str) -> anyhow::Result<()> {
    store::validate_account_name(name)?;
    let path = import_current_login(name).await?;
    println!(
        "Codex account {name:?} saved to {}. Add a name-only account entry to use it.",
        path.display()
    );
    Ok(())
}

async fn import_current_login(name: &str) -> anyhow::Result<PathBuf> {
    let source = crate::auth::default_codex_auth_path();
    let name = name.to_string();
    let source_display = source.display().to_string();
    tokio::task::spawn_blocking(move || {
        store::import_auth(&name, &source)
            .with_context(|| format!("failed to import {source_display}; run `codex login` first"))
    })
    .await
    .context("Codex credential import task failed")?
}
