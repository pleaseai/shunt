//! A pooled Antigravity weak tier whose first account stalls after its
//! headers. The idle gap that cuts the read is the gated call's bound, not the
//! account's fault, so the pool walk ends there: rotating would spend the gap
//! again on the next account and could replace the idle-marked error
//! `routing::serve` reads back with that account's own.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use shunt::config::{AuthMode, ProviderKind, UpstreamAuth};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::MockServer;

use super::harness::{
    anthropic_json, escalation_router, header, judge_text, messages_mock, post,
    unvalidated_gated_config, Tiers, GEMINI_UPSTREAM_MODEL,
};
use super::judge_harness::{
    alias, can_bind_loopback, env, start_gateway, upstream_with, CAPABLE_UPSTREAM_MODEL,
};
use super::DECLINE;

/// An upstream that answers every connection with `reply` and holds it open,
/// counting the connections it accepted.
async fn counting_stalled_upstream(reply: Vec<u8>) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let reply = reply.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let length = text[..head_end]
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if request.len() >= head_end + 4 + length {
                            break;
                        }
                    }
                }
                let _ = socket.write_all(&reply).await;
                let _ =
                    tokio::time::timeout(Duration::from_secs(10), socket.read(&mut chunk)).await;
            });
        }
    });
    (url, accepted)
}

fn write_account(dir: &std::path::Path, name: &str) {
    let expiry = (std::time::SystemTime::now() + Duration::from_secs(3600))
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec(&serde_json::json!({
            "access_token": format!("token-{name}"),
            "refresh_token": format!("refresh-{name}"),
            "expiry_date": expiry,
            "project_id": format!("proj-{name}"),
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Non-vacuity: drop the idle-marker early return from the first `Err` arm
/// of the pool loop in `gemini::forward` and the stall's `502` classifies as
/// `Rotate`, so the second account opens a second connection and the
/// connection-count assertion goes red.
#[tokio::test]
async fn a_pooled_antigravity_weak_body_stalled_after_its_headers_does_not_rotate_accounts() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = env().await;
    let dir = std::env::temp_dir().join(format!(
        "shunt-buffer-replay-pool-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let accounts_dir = dir.join("accounts");
    std::fs::create_dir_all(&accounts_dir).unwrap();
    write_account(&accounts_dir, "a");
    write_account(&accounts_dir, "b");
    vars.set("SHUNT_ANTIGRAVITY_ACCOUNTS_DIR", &accounts_dir);
    vars.set("SHUNT_ANTIGRAVITY_AUTH_FILE", dir.join("no-singleton.json"));

    let (strong, unused_tier, judge) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    messages_mock(anthropic_json(CAPABLE_UPSTREAM_MODEL, "STRONG"), 1)
        .mount(&strong)
        .await;
    messages_mock(judge_text(DECLINE), 0).mount(&judge).await;
    let mut reply =
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 4096\r\n\r\n"
            .to_vec();
    reply.extend_from_slice(
        b"{\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"WEAK-PAR",
    );
    let (stalled, accepted) = counting_stalled_upstream(reply).await;
    let tiers = Tiers {
        strong: strong.uri(),
        weak: unused_tier.uri(),
        responses: unused_tier.uri(),
        judge: judge.uri(),
    };
    let router = format!(
        "{}gated_idle_ms = 300\ngated_max_duration_ms = 8000\n",
        escalation_router("gemini-alias")
    );
    let mut config = unvalidated_gated_config(&tiers, &router);
    let mut gemini = upstream_with(
        "gemini",
        stalled,
        UpstreamAuth::Shorthand(AuthMode::AntigravityOauth),
    );
    gemini.kind = Some(ProviderKind::Antigravity);
    config.upstreams.push(gemini);
    config
        .models
        .push(alias("gemini-alias", "gemini", GEMINI_UPSTREAM_MODEL));
    let config = config
        .validate()
        .expect("the gated config with a pooled Antigravity tier is well formed");
    let gateway = start_gateway(config).await;

    let response = post(&gateway, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "x-gateway-route-source"),
        "escalation_fallback"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "STRONG", "got: {body}");
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "the idle cut must end the pool walk, not rotate to the next account"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
