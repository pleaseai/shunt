//! Resolution and validation of the optional `[server.auth] jwt_header` and
//! `jwt_cookie` slots: the extra places a verify-only JWT (see
//! [`crate::auth::inbound_jwt`]) may be presented. They exist for the Codex
//! CLI's native `gateway_oauth`, which delivers the identity-provider token in a
//! named header or cookie because Codex reserves `Authorization`. This module
//! resolves and validates the names and extracts a slot's value from a request;
//! deciding what the value is worth is the gate's job.

use axum::http::{header, HeaderMap, HeaderName};

use crate::auth::inbound::bearer_payload;

use crate::auth::slots::RESERVED_SLOTS;

/// The resolved JWT slots. Both `None` means the gate reads exactly the
/// credentials it read before these keys existed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct JwtSlots {
    pub(crate) header: Option<HeaderName>,
    /// Case-sensitive RFC 6265 cookie name.
    pub(crate) cookie: Option<String>,
}

/// Resolve `jwt_header` / `jwt_cookie`. `static_header` is the resolved
/// `[server.auth] header`, which the JWT header may not shadow. Errors are
/// `(key, message)` so the config layer can name the offending key.
pub(crate) fn resolve(
    header: Option<&str>,
    cookie: Option<&str>,
    has_jwt_entries: bool,
    static_header: &HeaderName,
) -> Result<JwtSlots, (&'static str, String)> {
    for (key, value) in [("jwt_header", header), ("jwt_cookie", cookie)] {
        if value.is_some() && !has_jwt_entries {
            return Err((
                key,
                "requires at least one [[server.auth.jwt]] entry".to_string(),
            ));
        }
    }
    let header = header
        .map(|value| resolve_header(value, static_header).map_err(|m| ("jwt_header", m)))
        .transpose()?;
    let cookie = cookie
        .map(|value| resolve_cookie(value).map_err(|m| ("jwt_cookie", m)))
        .transpose()?;
    Ok(JwtSlots { header, cookie })
}

/// What one JWT slot carries on a request, gathered with no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presented<'a> {
    Absent,
    One(&'a str),
    /// The slot appears more than once, so no single value can be trusted.
    Repeated,
    /// Present but not readable text; it can never verify.
    Unreadable,
}

/// Read the JWT out of header `name`: raw, or after a `Bearer` scheme in any
/// case.
pub(crate) fn header_credential<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Presented<'a> {
    let mut values = headers.get_all(name).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return if headers.contains_key(name) {
            Presented::Repeated
        } else {
            Presented::Absent
        };
    };
    let text = match bearer_payload(value) {
        Some(payload) => std::str::from_utf8(payload).ok(),
        None => value.to_str().ok().map(str::trim),
    };
    text.map_or(Presented::Unreadable, Presented::One)
}

/// Read the JWT out of the cookie `name`. `Cookie` fields are scanned as raw
/// bytes, never through `to_str`, so a byte outside visible ASCII in another
/// pair cannot hide a repeated `name`. HTTP/2 may split the header across
/// several fields, so every field counts toward the repeat check.
pub(crate) fn cookie_credential<'a>(headers: &'a HeaderMap, name: &str) -> Presented<'a> {
    let mut found = None;
    for field in headers.get_all(header::COOKIE) {
        for pair in field.as_bytes().split(|b| *b == b';') {
            let pair = pair.trim_ascii();
            let Some(eq) = pair.iter().position(|b| *b == b'=') else {
                continue;
            };
            if &pair[..eq] != name.as_bytes() {
                continue;
            }
            if found.replace(&pair[eq + 1..]).is_some() {
                return Presented::Repeated;
            }
        }
    }
    let Some(value) = found else {
        return Presented::Absent;
    };
    // RFC 6265 allows the value to be wrapped in one pair of double quotes.
    let value = match value {
        [b'"', inner @ .., b'"'] => inner,
        other => other,
    };
    match std::str::from_utf8(value) {
        Ok(text) if !text.is_empty() => Presented::One(text),
        _ => Presented::Unreadable,
    }
}

/// Whether the request may present a cookie credential (FR-12): a cookie rides
/// along on a cross-origin browser request without the page's consent, so the
/// slot is consulted only when the request carries no `Origin` or exactly one
/// that names this host under the request's own scheme (see [`request_scheme`]).
/// Anything unreadable, `null`, repeated, or unparseable fails closed.
pub(crate) fn cookie_slot_allowed(headers: &HeaderMap) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    match (origin.to_str(), host, request_scheme(headers)) {
        (Ok(origin), Some(host), Some(scheme)) => origin_matches_host(origin, host, Some(scheme)),
        _ => false,
    }
}

/// The scheme the client used to reach the gateway: the first value of
/// `X-Forwarded-Proto` when it is `http` or `https` (TLS is terminated by a
/// proxy in front of shunt's plain-HTTP listener), `http` when the header is
/// absent, and `None` for any other or unreadable value.
pub(crate) fn request_scheme(headers: &HeaderMap) -> Option<&'static str> {
    let Some(value) = headers.get("x-forwarded-proto") else {
        return Some("http");
    };
    let first = value.to_str().ok()?.split(',').next()?.trim();
    if first.eq_ignore_ascii_case("https") {
        Some("https")
    } else if first.eq_ignore_ascii_case("http") {
        Some("http")
    } else {
        None
    }
}

/// Whether the `Origin` header value is the same origin as the request: its
/// scheme equals the request `scheme`, and its host and port equal the `Host`
/// header value's, with `scheme`'s default port filled in when `Host` has none.
/// `None` compares host and port only, taking the Origin's own scheme as the
/// request's. `null` and unparseable origins never match.
pub(crate) fn origin_matches_host(origin: &str, host: &str, scheme: Option<&str>) -> bool {
    let Ok(origin) = url::Url::parse(origin) else {
        return false;
    };
    let scheme = scheme.unwrap_or(origin.scheme());
    if origin.scheme() != scheme {
        return false;
    }
    let Some(origin_host) = origin.host_str() else {
        return false;
    };
    let origin_port = origin.port_or_known_default();
    // Split a trailing `:port` only when it parses; a port-less IPv6 literal
    // (`[::1]`) keeps its whole text as the host name.
    let (host_without_port, host_port) = host
        .rsplit_once(':')
        .and_then(|(name, port)| Some((name, Some(port.parse::<u16>().ok()?))))
        .unwrap_or((host, None));
    // `Url::host_str` keeps an IPv6 literal's brackets and so does the Host
    // header; compare both unbracketed so either spelling matches.
    let unbracket = |host: &str| {
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned()
    };
    unbracket(origin_host).eq_ignore_ascii_case(&unbracket(host_without_port))
        && origin_port
            == host_port.or(match scheme {
                "http" => Some(80),
                "https" => Some(443),
                _ => None,
            })
}

/// Headers shunt itself reads or rewrites on the Codex path (session metadata,
/// framing, the cookie slot's Origin rule, the WebSocket handshake); a JWT
/// carried in one would leak into session ids or fail every turn's re-auth.
/// `originator` and `x-codex-routing-hint` come from the Codex CLI itself, and
/// `thread-id`, `x-client-request-id`, `x-codex-window-id` are synthesized by
/// shunt, so a slot with one of those names would collide with a real value.
const CODEX_PATH_HEADERS: &[&str] = &[
    "session-id",
    "x-claude-code-session-id",
    "content-type",
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "host",
    "origin",
    "upgrade",
    "connection",
    "openai-beta",
    "thread-id",
    "x-client-request-id",
    "x-codex-window-id",
    "originator",
    "x-codex-routing-hint",
];

fn resolve_header(value: &str, static_header: &HeaderName) -> Result<HeaderName, String> {
    if value.trim().is_empty() {
        return Err("must not be empty".to_string());
    }
    let name = HeaderName::from_bytes(value.as_bytes())
        .map_err(|_| format!("{value:?} is not a valid header name"))?;
    let reserved = name == "authorization"
        || name == "x-api-key"
        || RESERVED_SLOTS.iter().any(|slot| name == *slot);
    if reserved {
        return Err(format!(
            "{name} is a credential header shunt already reads or strips; choose another name"
        ));
    }
    if CODEX_PATH_HEADERS.iter().any(|reserved| name == *reserved)
        || name.as_str().starts_with("sec-websocket-")
    {
        return Err(format!(
            "{name} is a header shunt reads or rewrites on the Codex endpoint; choose another name"
        ));
    }
    if name == *static_header {
        return Err(format!("{name} is already the [server.auth] header"));
    }
    Ok(name)
}

/// RFC 6265 `cookie-name` is an RFC 2616 `token`: one or more `tchar`.
fn resolve_cookie(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("must not be empty".to_string());
    }
    let is_tchar = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    if !value.bytes().all(is_tchar) {
        return Err(format!("{value:?} is not a valid cookie name"));
    }
    if value.eq_ignore_ascii_case(crate::admin::SESSION_COOKIE) {
        return Err(format!(
            "{value} is the admin dashboard's session cookie; choose another name"
        ));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests;
