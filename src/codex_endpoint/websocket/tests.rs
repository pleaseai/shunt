use super::*;

/// Run the check against a handshake carrying the given `Origin` / `Host`.
fn allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let mut headers = HeaderMap::new();
    if let Some(origin) = origin {
        headers.insert(header::ORIGIN, origin.parse().unwrap());
    }
    if let Some(host) = host {
        headers.insert(header::HOST, host.parse().unwrap());
    }
    same_origin_or_non_browser(&headers)
}

#[test]
fn same_origin_accepts_ipv6_with_and_without_port() {
    assert!(allowed(Some("http://[::1]:3001"), Some("[::1]:3001")));
    assert!(allowed(Some("http://[::1]"), Some("[::1]")));
}

#[test]
fn same_origin_rejects_ipv6_host_mismatch() {
    assert!(!allowed(Some("http://[::2]:3001"), Some("[::1]:3001")));
}

#[test]
fn same_origin_accepts_ipv4_and_rejects_port_mismatch() {
    assert!(allowed(
        Some("http://127.0.0.1:3001"),
        Some("127.0.0.1:3001")
    ));
    assert!(!allowed(
        Some("http://127.0.0.1:3002"),
        Some("127.0.0.1:3001")
    ));
}

#[test]
fn missing_origin_is_non_browser_and_missing_host_is_rejected() {
    assert!(allowed(None, Some("127.0.0.1:3001")));
    assert!(!allowed(Some("http://127.0.0.1:3001"), None));
}

/// The `OpenAI-Beta` an HTTP turn forwards for handshake values `betas`
/// (each a separate header field), or `None` when it carries none.
fn http_beta(betas: &[&str]) -> Option<String> {
    let mut headers = HeaderMap::new();
    for beta in betas {
        headers.append("openai-beta", beta.parse().unwrap());
    }
    http_openai_beta(&mut headers);
    assert!(headers.get_all("openai-beta").iter().count() <= 1);
    headers
        .get("openai-beta")
        .map(|value| value.to_str().unwrap().to_owned())
}

#[test]
fn openai_beta_swaps_only_the_websocket_selector() {
    assert_eq!(
        http_beta(&["responses_websockets=2026-02-06"]).as_deref(),
        Some("responses=experimental")
    );
    assert_eq!(
        http_beta(&["responses_websockets=2026-02-06, assistants=v2"]).as_deref(),
        Some("assistants=v2, responses=experimental")
    );
    assert_eq!(
        http_beta(&["responses=experimental"]).as_deref(),
        Some("responses=experimental")
    );
}

#[test]
fn openai_beta_merges_repeated_fields_and_stays_absent_when_absent() {
    assert_eq!(
        http_beta(&["responses_websockets=2026-02-06", "assistants=v2"]).as_deref(),
        Some("assistants=v2, responses=experimental")
    );
    assert_eq!(http_beta(&[]), None);
}
