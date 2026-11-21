use axum::http::{header, HeaderMap, HeaderName};

const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "host",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "upgrade",
];

pub fn filtered(headers: &HeaderMap) -> HeaderMap {
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        if !is_hop_by_hop(name) {
            forwarded.append(name, value.clone());
        }
    }
    forwarded
}

pub fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP_HEADERS
        .iter()
        .any(|header| name.as_str().eq_ignore_ascii_case(header))
}

/// Whether a request either came from outside a browser (no `Origin`) or from
/// a page served by this same origin (`Origin`'s host and port equal `Host`'s).
///
/// Browsers attach `Origin` to every cross-origin request that can carry a body,
/// and a page cannot forge or remove it, so a request that fails this check was
/// sent by another site's page. `Origin: null` (a sandboxed frame, a `data:` URL)
/// fails it too. On its own this does not stop a rebinding attack, where the
/// attacker's name resolves to this host and `Origin` matches `Host`.
pub(crate) fn same_origin_or_non_browser(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    // Host and port only, as documented for the unauthenticated endpoint, so
    // `http` and `https` on one host are not told apart here (the cookie slot
    // does compare them).
    crate::auth::jwt_slots::origin_matches_host(origin, host, None)
}

/// Request headers that describe the browser page a request came from. They are
/// dropped before a request is forwarded: shunt calls the upstream as a server,
/// not on behalf of that page. Anthropic answers an OAuth request that carries
/// `Origin` with `401 authentication_error` ("CORS requests must set
/// 'anthropic-dangerous-direct-browser-access' header"), which the pool would
/// take for a dead credential and answer with a forced refresh and a cooldown.
const BROWSER_CONTEXT_HEADERS: &[&str] = &["origin", "referer"];

/// Remove [`BROWSER_CONTEXT_HEADERS`] and every `Sec-Fetch-*` header.
pub(crate) fn strip_browser_context(headers: &mut HeaderMap) {
    for name in BROWSER_CONTEXT_HEADERS {
        headers.remove(*name);
    }
    let fetch_metadata: Vec<HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with("sec-fetch-"))
        .cloned()
        .collect();
    for name in fetch_metadata {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_browser_context_drops_origin_referer_and_fetch_metadata_only() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("origin", "https://evil.example"),
            ("referer", "https://evil.example/page"),
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-mode", "no-cors"),
            ("sec-fetch-dest", "empty"),
            ("anthropic-version", "2023-06-01"),
            ("user-agent", "claude-cli/2.1.0"),
        ] {
            headers.append(name, value.parse().unwrap());
        }
        strip_browser_context(&mut headers);
        let mut kept: Vec<_> = headers.keys().map(HeaderName::as_str).collect();
        kept.sort_unstable();
        assert_eq!(kept, ["anthropic-version", "user-agent"]);
    }
}
