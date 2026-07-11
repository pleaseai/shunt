use std::time::Duration;

use anyhow::{bail, Context};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::auth::{
    cursor_auth::{parse_token_response, write_auth},
    default_cursor_auth_path,
};

pub async fn run(provider: &str) -> anyhow::Result<()> {
    if provider != "cursor" {
        bail!("unknown Cursor login provider {provider:?}");
    }
    let base_url = std::env::var("SHUNT_CURSOR_BASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://api2.cursor.sh".to_string());
    run_with_base(&base_url).await
}

pub async fn run_with_base(base_url: &str) -> anyhow::Result<()> {
    let mut random = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut random);
    let verifier = URL_SAFE_NO_PAD.encode(random);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let uuid = uuid::Uuid::new_v4().to_string();
    let login_url = format!(
        "https://cursor.com/loginDeepControl?challenge={challenge}&uuid={uuid}&mode=login&redirectTarget=cli"
    );
    println!("Open this URL to authenticate with Cursor:\n\n    {login_url}\n");
    if let Err(error) = open_url(&login_url) {
        eprintln!("Could not open browser automatically: {error}");
    }

    let client = reqwest::Client::new();
    let tokens = poll(&client, base_url, &uuid, &verifier).await?;
    let path = default_cursor_auth_path();
    // Offload the blocking credential write to the blocking thread pool so it
    // never stalls the async runtime (mirrors CursorAuthStore::write_auth_off_thread).
    let write_path = path.clone();
    tokio::task::spawn_blocking(move || write_auth(&write_path, &tokens))
        .await
        .map_err(|error| anyhow::anyhow!("Cursor auth write task failed: {error}"))?
        .with_context(|| format!("failed to write Cursor credentials to {}", path.display()))?;
    println!("Login successful. Credentials saved to {}", path.display());
    Ok(())
}

async fn poll(
    client: &reqwest::Client,
    base_url: &str,
    uuid: &str,
    verifier: &str,
) -> anyhow::Result<crate::auth::cursor_auth::StoredCursorAuth> {
    let mut delay = Duration::from_secs(1);
    let mut last_error: Option<reqwest::Error> = None;
    for _ in 0..150 {
        let response = match client
            .get(format!(
                "{}/auth/poll?uuid={uuid}&verifier={verifier}",
                base_url.trim_end_matches('/')
            ))
            .header("content-type", "application/json")
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                // A transient network error must not abort the login — the user
                // may still be completing the browser flow. Keep polling, but
                // remember the error so a persistent connectivity failure isn't
                // masked by the generic timeout below.
                last_error = Some(error);
                tokio::time::sleep(delay).await;
                delay = (delay.mul_f32(1.2)).min(Duration::from_secs(10));
                continue;
            }
        };
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            tokio::time::sleep(delay).await;
            delay = (delay.mul_f32(1.2)).min(Duration::from_secs(10));
            continue;
        }
        let status = response.status();
        let text = response
            .text()
            .await
            .context("invalid Cursor poll response")?;
        let value: Value = serde_json::from_str(&text).context("invalid Cursor poll response")?;
        if !status.is_success() {
            bail!("Cursor login poll failed (HTTP {status}): {value}");
        }
        return parse_token_response(&value)
            .ok_or_else(|| anyhow::anyhow!("Cursor login response missing accessToken"));
    }
    match last_error {
        Some(error) => Err(anyhow::Error::new(error).context(
            "Cursor login timed out after repeated network errors; \
             run shunt login cursor to try again",
        )),
        None => bail!("Cursor login timed out; run shunt login cursor to try again"),
    }
}

fn open_url(url: &str) -> anyhow::Result<()> {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()?
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .status()?
    } else {
        std::process::Command::new("xdg-open").arg(url).status()?
    };
    if !status.success() {
        bail!("browser open command exited with {status}");
    }
    Ok(())
}
