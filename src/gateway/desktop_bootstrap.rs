//! `GET /user/bootstrap` — the Claude Desktop bootstrap-config document.
//!
//! The operator supplies a JSON template file (`[server.gateway.desktop_bootstrap]`);
//! it is re-read per request and `${user.email}`, `${user.sub}` and `${user.name}`
//! in string values are replaced with the caller's token claims. The document is
//! otherwise passed through untouched (Desktop accepts both the nested
//! `bootstrap-config-v2` and the flat v1 shapes). Every response carries
//! `Cache-Control: no-store` and none is ever a redirect.

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::Value;

use super::{
    jwt::Claims,
    managed::{if_none_match, sha256_label},
};
use crate::{error::ShuntError, server::AppState};

pub async fn get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    no_store(respond(state, &headers).await)
}

async fn respond(state: AppState, headers: &HeaderMap) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth else {
        return not_found();
    };
    let Some(template) = auth.desktop_bootstrap_template() else {
        return not_found();
    };
    let Some(claims) = auth.authenticate_bearer(headers) else {
        return ShuntError::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing or invalid gateway bearer token",
        )
        .into_response();
    };
    if !auth.entitled(&claims.email) {
        return ShuntError::new(
            StatusCode::FORBIDDEN,
            "permission_error",
            "this account is not entitled to a Claude Desktop bootstrap config",
        )
        .into_response();
    }

    let document = match tokio::fs::read(template).await {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(document @ Value::Object(_)) => Ok(document),
            Ok(_) => Err("not a JSON object".to_string()),
            Err(error) => Err(error.to_string()),
        },
        Err(error) => Err(error.to_string()),
    };
    let mut document = match document {
        Ok(document) => document,
        Err(error) => {
            tracing::warn!(
                path = %template.display(),
                %error,
                "Claude Desktop bootstrap template is unusable"
            );
            return ShuntError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "Claude Desktop bootstrap config is unavailable",
            )
            .into_response();
        }
    };
    substitute(&mut document, &claims);

    let body = serde_json::to_vec(&document).expect("JSON value serializes");
    let checksum = sha256_label(&body);
    let etag = HeaderValue::from_str(&format!("\"{checksum}\""))
        .expect("SHA-256 ETag is a valid header value");
    if if_none_match(headers, &checksum) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(header::ETAG, etag);
        return response;
    }
    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response.headers_mut().insert(header::ETAG, etag);
    response
}

fn not_found() -> Response {
    ShuntError::new(
        StatusCode::NOT_FOUND,
        "not_found_error",
        "no Claude Desktop bootstrap template",
    )
    .into_response()
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Replace the placeholders in every string value (never in object keys).
fn substitute(value: &mut Value, claims: &Claims) {
    match value {
        Value::String(text) => {
            if text.contains("${") {
                *text = substitute_str(text, claims);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| substitute(item, claims)),
        Value::Object(map) => map.values_mut().for_each(|item| substitute(item, claims)),
        _ => {}
    }
}

/// Single left-to-right pass, so a substituted claim that itself looks like a
/// placeholder is never expanded again. Unknown `${...}` tokens stay verbatim.
fn substitute_str(text: &str, claims: &Claims) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let replacement = [
            ("${user.email}", &claims.email),
            ("${user.sub}", &claims.sub),
            ("${user.name}", &claims.name),
        ]
        .into_iter()
        .find(|(token, _)| rest.starts_with(token));
        match replacement {
            Some((token, claim)) => {
                out.push_str(claim);
                rest = &rest[token.len()..];
            }
            None => {
                out.push_str("${");
                rest = &rest[2..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::substitute_str;
    use crate::gateway::jwt::Claims;

    fn claims(email: &str) -> Claims {
        Claims {
            sub: "sub-1".into(),
            email: email.into(),
            name: "Dev".into(),
            aud: String::new(),
            iss: String::new(),
            iat: 0,
            exp: 0,
            shunt_token_use: String::new(),
        }
    }

    #[test]
    fn claims_that_look_like_placeholders_are_not_expanded_again() {
        let out = substitute_str("${user.email}|${user.sub}|${other}", &claims("${user.sub}"));
        assert_eq!(out, "${user.sub}|sub-1|${other}");
    }
}
