use std::net::SocketAddr;

use axum::{
    extract::{rejection::FormRejection, ConnectInfo, Query, State},
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
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    device_page(auth_page(&auth, &query.user_code, None, false))
}

pub async fn post(
    State(state): State<AppState>,
    connection: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    form: Result<Form<DeviceForm>, FormRejection>,
) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !same_origin(&headers, auth.public_url()) {
        let user_code = form
            .as_ref()
            .ok()
            .map(|Form(form)| form.user_code.as_str())
            .unwrap_or_default();
        return device_page(auth_page(
            &auth,
            user_code,
            Some("This request came from another site and was blocked."),
            false,
        ));
    }
    let Form(form) = match form {
        Ok(form) => form,
        Err(_) => {
            return device_page(auth_page(
                &auth,
                "",
                Some("The submitted form could not be read. Try again."),
                false,
            ));
        }
    };
    let peer = connection.map(|Extension(ConnectInfo(address))| address);
    let client_ip = client_ip(&headers, peer, auth.trust_forwarded_for());
    if !state
        .gateway_stores
        .device_verify_rate
        .check(client_ip.as_str())
    {
        return device_page(auth_page(
            &auth,
            &form.user_code,
            Some("Too many attempts. Wait a minute and try again."),
            false,
        ));
    }
    let user_code = normalize_user_code(&form.user_code);
    let Some(approval) = auth.approval_provider() else {
        return device_page(auth_page(
            &auth,
            &user_code,
            Some("Password sign-in is disabled on this gateway; use the sign-in button."),
            false,
        ));
    };
    let Some(identity) = approval.verify(&form.login, &form.secret) else {
        return device_page(auth_page(
            &auth,
            &user_code,
            Some("The login or secret was not accepted."),
            false,
        ));
    };
    if !state
        .gateway_stores
        .device_grants
        .approve(&user_code, identity)
    {
        return device_page(auth_page(
            &auth,
            &user_code,
            Some("The device code is invalid, expired, or already used."),
            false,
        ));
    }
    device_page(auth_page(
        &auth,
        &user_code,
        Some("Device approved. You can return to your device."),
        true,
    ))
}

pub(super) fn device_page(body: String) -> Response {
    const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; \
base-uri 'none'; frame-ancestors 'none'";
    (
        [
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Html(body),
    )
        .into_response()
}

pub(super) fn same_origin(headers: &HeaderMap, public_url: &str) -> bool {
    let fetch_site = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok());
    if fetch_site.is_some_and(|site| site.eq_ignore_ascii_case("cross-site")) {
        return false;
    }

    let mut has_origin_signal = false;
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        has_origin_signal = true;
        if !same_url_origin(origin, public_url) {
            return false;
        }
    }
    if let Some(referer) = headers
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
    {
        has_origin_signal = true;
        if !same_url_origin(referer, public_url) {
            return false;
        }
    }
    has_origin_signal
        || fetch_site.is_some_and(|site| {
            matches!(
                site.to_ascii_lowercase().as_str(),
                "same-origin" | "same-site" | "none"
            )
        })
}

fn same_url_origin(candidate: &str, public_url: &str) -> bool {
    let Ok(candidate) = reqwest::Url::parse(candidate) else {
        return false;
    };
    let Ok(public_url) = reqwest::Url::parse(public_url) else {
        return false;
    };
    candidate.origin() == public_url.origin()
}

pub(super) fn client_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trust_forwarded_for: bool,
) -> String {
    if trust_forwarded_for {
        if let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                headers
                    .get("x-real-ip")
                    .and_then(|value| value.to_str().ok())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
        {
            return forwarded.to_string();
        }
    }
    peer.map(|address| address.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

pub(super) fn normalize_user_code(code: &str) -> String {
    let compact: String = code
        .trim()
        .chars()
        .filter(|character| *character != '-')
        .flat_map(char::to_uppercase)
        .collect();
    if compact.is_ascii() && compact.len() == 8 {
        format!("{}-{}", &compact[..4], &compact[4..])
    } else {
        compact
    }
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

pub(super) fn auth_page(
    auth: &super::GatewayAuth,
    user_code: &str,
    notice: Option<&str>,
    success: bool,
) -> String {
    page(
        user_code,
        notice,
        success,
        auth.oidc().map(super::ResolvedIdp::button_label),
        auth.approval_provider().is_some(),
    )
}

pub(super) fn page(
    user_code: &str,
    notice: Option<&str>,
    success: bool,
    oidc_label: Option<&str>,
    password_enabled: bool,
) -> String {
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
    let forms = if success {
        String::new()
    } else {
        let sso_form = oidc_label
            .map(|label| {
                format!(
                    r#"<form method="post" action="/device/authorize">
<label for="sso-user-code">Device code</label>
<input id="sso-user-code" name="user_code" value="{user_code}" autocomplete="one-time-code" spellcheck="false" required autofocus>
<button type="submit">{}</button>
</form>"#,
                    escape_html(label)
                )
            })
            .unwrap_or_default();
        let password_form = if password_enabled {
            format!(
                r#"<form method="post" action="/device">
<label for="user-code">Device code</label>
<input id="user-code" name="user_code" value="{user_code}" autocomplete="one-time-code" spellcheck="false" required{}>
<label for="login">Email</label>
<input id="login" name="login" type="email" autocomplete="username" required>
<label for="current-password">Secret</label>
<input id="current-password" name="secret" type="password" autocomplete="current-password" required enterkeyhint="done">
<button type="submit">Approve device</button>
</form>"#,
                if oidc_label.is_none() {
                    " autofocus"
                } else {
                    ""
                }
            )
        } else {
            String::new()
        };
        format!("{sso_form}{password_form}")
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
{notice}{forms}</div></main></body></html>"#
    )
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use axum::http::{header, HeaderMap, HeaderValue};

    use super::{client_ip, device_page, normalize_user_code, page, same_origin};

    #[test]
    fn page_escapes_prefilled_code_and_never_auto_submits() {
        let html = page("<script>", None, false, None, true);
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script"));
        assert!(html.contains("method=\"post\""));
    }

    #[test]
    fn device_page_sets_browser_security_headers() {
        let response = device_page(page("ABCD-EFGH", None, false, None, true));
        let headers = response.headers();

        assert_eq!(
            headers.get(header::CONTENT_SECURITY_POLICY).unwrap(),
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
        );
        assert_eq!(
            headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(headers.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        assert_eq!(headers.get(header::REFERRER_POLICY).unwrap(), "no-referrer");
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    }

    #[test]
    fn user_code_normalization_accepts_omitted_separator() {
        assert_eq!(normalize_user_code(" bcdfghjk "), "BCDF-GHJK");
        assert_eq!(normalize_user_code("bcdf-ghjk"), "BCDF-GHJK");
        assert_eq!(normalize_user_code("éééé"), "ÉÉÉÉ");
    }

    #[test]
    fn forwarded_addresses_require_explicit_trust() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.8, 192.0.2.3"),
        );
        headers.insert("x-real-ip", HeaderValue::from_static("198.51.100.9"));
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 4)), 43123);

        assert_eq!(client_ip(&headers, Some(peer), false), "203.0.113.4");
        assert_eq!(client_ip(&headers, Some(peer), true), "198.51.100.8");
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
        headers.insert(
            header::REFERER,
            HeaderValue::from_static("https://gateway.example/device"),
        );
        assert!(same_origin(&headers, "https://gateway.example"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        assert!(!same_origin(&headers, "https://gateway.example"));

        let mut contradictory = HeaderMap::new();
        contradictory.insert("sec-fetch-site", HeaderValue::from_static("same-site"));
        contradictory.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://sibling.example"),
        );
        assert!(!same_origin(&contradictory, "https://gateway.example"));

        let mut navigation = HeaderMap::new();
        navigation.insert("sec-fetch-site", HeaderValue::from_static("none"));
        assert!(same_origin(&navigation, "https://gateway.example"));
        assert!(!same_origin(&HeaderMap::new(), "https://gateway.example"));
    }
}
