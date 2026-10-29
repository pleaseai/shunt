//! Resolution and validation of the optional `[server.auth] jwt_header` and
//! `jwt_cookie` slots: the extra places a verify-only JWT (see
//! [`crate::auth::inbound_jwt`]) may be presented, for deployments behind an
//! identity-aware proxy that injects the token somewhere other than
//! `Authorization`. This module only resolves and validates the names; reading
//! the slots on a request is the gate's job.

use axum::http::HeaderName;

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
    Ok(value.to_string())
}
