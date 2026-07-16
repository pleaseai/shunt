use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Extension, Form,
};
use serde::Deserialize;

use crate::server::AppState;

#[derive(Default, Deserialize)]
pub struct DeviceQuery {
    #[serde(default)]
    user_code: String,
}

#[derive(Default, Deserialize)]
pub struct DeviceForm {
    #[serde(default)]
    user_code: String,
    #[serde(default)]
    login: String,
    #[serde(default)]
    secret: String,
}

pub async fn get(State(state): State<AppState>, Query(query): Query<DeviceQuery>) -> Response {
    if state.refreshed().gateway_auth.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    Html(page(&query.user_code, None, false)).into_response()
}

pub async fn post(
    State(state): State<AppState>,
    connection: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Form(form): Form<DeviceForm>,
) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !same_origin(&headers, auth.public_url()) {
        return Html(page(
            &form.user_code,
            Some("This request came from another site and was blocked."),
            false,
        ))
        .into_response();
    }
    let peer = connection.map(|Extension(ConnectInfo(address))| address);
    let client_ip = client_ip(&headers, peer);
    if !state
        .gateway_stores
        .device_verify_rate
        .check(client_ip.as_str())
    {
        return Html(page(
            &form.user_code,
            Some("Too many attempts. Wait a minute and try again."),
            false,
        ))
        .into_response();
    }
    let user_code = normalize_user_code(&form.user_code);
    let Some(identity) = auth.approval_provider().verify(&form.login, &form.secret) else {
        return Html(page(
            &user_code,
            Some("The login or secret was not accepted."),
            false,
        ))
        .into_response();
    };
    if !state
        .gateway_stores
        .device_grants
        .approve(&user_code, identity)
    {
        return Html(page(
            &user_code,
            Some("The device code is invalid, expired, or already used."),
            false,
        ))
        .into_response();
    }
    Html(page(
        &user_code,
        Some("Device approved. You can return to your device."),
        true,
    ))
    .into_response()
}

fn same_origin(headers: &HeaderMap, public_url: &str) -> bool {
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        if matches!(site, "same-origin" | "same-site") {
            return true;
        }
        if site.eq_ignore_ascii_case("cross-site") {
            return false;
        }
    }
    let expected = public_url.trim_end_matches('/');
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        return origin.trim_end_matches('/').eq_ignore_ascii_case(expected);
    }
    if let Some(referer) = headers
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
    {
        return referer == expected
            || referer
                .strip_prefix(expected)
                .is_some_and(|suffix| suffix.starts_with('/'));
    }
    headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|site| site.eq_ignore_ascii_case("none"))
        && !headers.contains_key("sec-fetch-mode")
        && !headers.contains_key("sec-fetch-dest")
}

fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    // `X-Forwarded-For` is honored for reverse-proxy deployments; operators must
    // strip client-supplied forwarding headers at the trusted edge.
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|value| value.to_str().ok())
        })
        .map(ToOwned::to_owned)
        .or_else(|| peer.map(|address| address.ip().to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

fn normalize_user_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

fn escape_html(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#x27;"),
            _ => output.push(ch),
        }
    }
    output
}

fn page(user_code: &str, notice: Option<&str>, success: bool) -> String {
    let user_code = escape_html(user_code);
    let notice = notice
        .map(|message| {
            format!(
                "<div class=\"notice {}\" role=\"status\">{}</div>",
                if success { "ok" } else { "error" },
                escape_html(message)
            )
        })
        .unwrap_or_default();
    let form = if success {
        String::new()
    } else {
        format!(
            r#"<form method="post" action="/device">
<label for="user-code">Device code</label>
<input id="user-code" name="user_code" value="{user_code}" autocomplete="one-time-code" spellcheck="false" required autofocus>
<label for="login">Email</label>
<input id="login" name="login" type="email" autocomplete="username" required>
<label for="current-password">Secret</label>
<input id="current-password" name="secret" type="password" autocomplete="current-password" required enterkeyhint="done">
<button type="submit">Approve device</button>
</form>"#
        )
    };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>shunt gateway — approve device</title><style>
:root {{ color-scheme: light dark; }} * {{ box-sizing: border-box; }}
body {{ margin: 0; background: #f6f7f9; color: #1a1a1a; font: 1rem/1.5 system-ui, sans-serif; }}
main {{ max-width: 28rem; margin: 8vh auto; padding: 1rem; }}
.card {{ background: canvas; border: 1px solid #8885; border-radius: .75rem; padding: 1.25rem; }}
h1 {{ margin-top: 0; font-size: 1.4rem; }} label {{ display: block; margin-top: .9rem; font-weight: 600; }}
input, button {{ width: 100%; min-height: 3rem; margin-top: .25rem; padding: .65rem; border-radius: .5rem; font: inherit; }}
input {{ border: 1px solid #7778; background: canvas; color: inherit; }}
button {{ margin-top: 1.2rem; border: 1px solid #315ee8; background: #315ee8; color: white; cursor: pointer; }}
input:focus-visible, button:focus-visible {{ outline: 3px solid #315ee8; outline-offset: 2px; }}
.notice {{ margin-bottom: 1rem; padding: .7rem; border-radius: .5rem; }}
.notice.error {{ background: #c0392b22; }} .notice.ok {{ background: #27864d22; }}
@media (prefers-color-scheme: dark) {{ body {{ background: #16181d; color: #eee; }} }}
</style></head><body><main><div class="card"><h1>Approve this device</h1>
<p>Enter the code shown by Claude Code, then sign in with a gateway account.</p>
{notice}{form}</div></main></body></html>"#
    )
}

#[cfg(test)]
mod tests {
    use axum::http::{header, HeaderMap, HeaderValue};

    use super::{page, same_origin};

    #[test]
    fn page_escapes_prefilled_code_and_never_auto_submits() {
        let html = page("<script>", None, false);
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script"));
        assert!(html.contains("method=\"post\""));
    }

    #[test]
    fn csrf_accepts_same_origin_signals_and_rejects_cross_site() {
        let mut headers = HeaderMap::new();
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(same_origin(&headers, "https://gateway.example"));

        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!same_origin(&headers, "https://gateway.example"));

        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://gateway.example"),
        );
        assert!(same_origin(&headers, "https://gateway.example"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        assert!(!same_origin(&headers, "https://gateway.example"));
    }
}
