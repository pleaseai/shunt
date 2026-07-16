use std::time::Duration;

use anyhow::{bail, Context};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::auth::default_cursor_auth_path;

use super::auth::{parse_token_response, write_auth};

pub async fn run(provider: &str) -> anyhow::Result<()> {
    if provider != "cursor" {
        bail!("unknown Cursor login provider {provider:?}");
    }
    let base_url = super::resolve_base_url("https://api2.cursor.sh");
    run_with_base(&base_url).await
}

pub async fn run_with_base(base_url: &str) -> anyhow::Result<()> {
    let mut random = [0_u8; 32];
    rand::rng().fill_bytes(&mut random);
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
) -> anyhow::Result<super::auth::StoredCursorAuth> {
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
        // Open via rundll32 FileProtocolHandler rather than `cmd /c start`: the
        // login URL contains `&` query separators, which cmd.exe would treat as
        // command separators and truncate the URL.
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .status()?
    } else {
        std::process::Command::new("xdg-open").arg(url).status()?
    };
    if !status.success() {
        bail!("browser open command exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn poll_returns_tokens_on_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/auth/poll"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accessToken": "access-1",
                "refreshToken": "refresh-1"
            })))
            .mount(&server)
            .await;

        let auth = poll(
            &reqwest::Client::new(),
            &server.uri(),
            "uuid-1",
            "verifier-1",
        )
        .await
        .unwrap();
        assert_eq!(auth.access_token, "access-1");
        assert_eq!(auth.refresh_token.as_deref(), Some("refresh-1"));
    }

    #[tokio::test]
    async fn poll_retries_while_pending_then_succeeds() {
        let server = MockServer::start().await;
        // First call: 404 (login not completed yet); the poll loop retries.
        Mock::given(method("GET"))
            .and(path("/auth/poll"))
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/auth/poll"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accessToken": "access-2"
            })))
            .mount(&server)
            .await;

        let auth = poll(
            &reqwest::Client::new(),
            &server.uri(),
            "uuid-2",
            "verifier-2",
        )
        .await
        .unwrap();
        assert_eq!(auth.access_token, "access-2");
    }

    #[tokio::test]
    async fn poll_fails_on_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/auth/poll"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "bad request"
            })))
            .mount(&server)
            .await;

        let error = poll(
            &reqwest::Client::new(),
            &server.uri(),
            "uuid-3",
            "verifier-3",
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Cursor login poll failed"));
    }
}
