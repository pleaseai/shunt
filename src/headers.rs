use axum::http::{HeaderMap, HeaderName};

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
