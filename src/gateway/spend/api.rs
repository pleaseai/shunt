use std::time::SystemTime;

use axum::{
    extract::{FromRequest, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use super::store::{
    validate_amount, validate_scope, Period, Scope, SpendLimit, MAX_AMOUNT_LENGTH,
    MAX_USER_ID_LENGTH,
};
use crate::{auth::inbound::constant_time_eq, server::AppState};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    after_id: Option<String>,
    before_id: Option<String>,
    scope_type: Option<String>,
}

fn default_limit() -> usize {
    20
}

#[derive(Debug, Serialize)]
struct ListResponse {
    data: Vec<SpendLimit>,
    has_more: bool,
    first_id: Option<String>,
    last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    amount: serde_json::Value,
    scope: serde_json::Value,
    #[serde(default)]
    period: Period,
    currency: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeletedResponse {
    id: String,
    #[serde(rename = "type")]
    object_type: &'static str,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    #[serde(rename = "type")]
    object_type: &'static str,
    error: ErrorDetail,
    request_id: String,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    #[serde(rename = "type")]
    error_type: &'static str,
    message: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    Write,
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<ListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let request_id = request_id();
    let state = state.refreshed();
    if let Err(response) = authenticate(&state, &headers, false, &request_id) {
        return *response;
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("invalid query parameters: {rejection}"),
                request_id,
            );
        }
    };
    if !(1..=1000).contains(&query.limit) {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "limit must be between 1 and 1000",
            request_id,
        );
    }
    if query.after_id.is_some() && query.before_id.is_some() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "after_id and before_id are mutually exclusive",
            request_id,
        );
    }
    let scope_filter = match query.scope_type.as_deref() {
        None => None,
        Some("user") => Some("user"),
        Some("organization") => Some("organization"),
        Some(scope_type @ ("rbac_group" | "seat_tier" | "organization_service")) => {
            return unsupported_scope(scope_type, request_id);
        }
        Some(other) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("invalid scope_type {other:?}"),
                request_id,
            );
        }
    };
    let limits: Vec<_> = state
        .gateway_stores
        .spend
        .list()
        .into_iter()
        .filter(|limit| {
            matches!(
                (scope_filter, &limit.scope),
                (None, _)
                    | (Some("user"), Scope::User { .. })
                    | (Some("organization"), Scope::Organization)
            )
        })
        .collect();
    let page = match paginate(limits, &query) {
        Ok(page) => page,
        Err(message) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                message,
                request_id,
            );
        }
    };
    success(StatusCode::OK, &page, request_id)
}

pub async fn create(State(state): State<AppState>, request: Request) -> Response {
    let request_id = request_id();
    let state = state.refreshed();
    let actor = match authenticate(&state, request.headers(), true, &request_id) {
        Ok(actor) => actor,
        Err(response) => return *response,
    };
    let Json(body) = match Json::<CreateRequest>::from_request(request, &state).await {
        Ok(body) => body,
        Err(rejection) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("invalid JSON body: {rejection}"),
                request_id,
            );
        }
    };
    if body.currency.as_deref().is_some_and(|value| value != "USD") {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "currency must be USD",
            request_id,
        );
    }
    let amount = match body.amount {
        serde_json::Value::Null => None,
        serde_json::Value::String(amount) if validate_amount(Some(&amount)).is_ok() => Some(amount),
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "amount must be a whole-number string of USD cents with at most {MAX_AMOUNT_LENGTH} digits or null"
                ),
                request_id,
            );
        }
    };
    let scope = match parse_scope(body.scope) {
        Ok(scope) => scope,
        Err(ScopeError::Unsupported(scope_type)) => {
            return unsupported_scope(&scope_type, request_id);
        }
        Err(ScopeError::Invalid(message)) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                message,
                request_id,
            );
        }
    };
    let _gate = state.gateway_stores.spend.mutation_gate().await;
    let previous = state.gateway_stores.spend.export();
    let limit = state
        .gateway_stores
        .spend
        .upsert(scope, body.period, amount, &actor, timestamp());
    if let Err(message) = super::persist::save(&state).await {
        state.gateway_stores.spend.replace(previous);
        tracing::error!(%message);
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "failed to persist spend limit",
            request_id,
        );
    }
    success(StatusCode::OK, &limit, request_id)
}

pub async fn get_by_id(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let request_id = request_id();
    let state = state.refreshed();
    if let Err(response) = authenticate(&state, &headers, false, &request_id) {
        return *response;
    }
    match state.gateway_stores.spend.get(&id) {
        Some(limit) => success(StatusCode::OK, &limit, request_id),
        None => not_found(&id, request_id),
    }
}

pub async fn delete_by_id(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let request_id = request_id();
    let state = state.refreshed();
    let actor = match authenticate(&state, &headers, true, &request_id) {
        Ok(actor) => actor,
        Err(response) => return *response,
    };
    let _gate = state.gateway_stores.spend.mutation_gate().await;
    let previous = state.gateway_stores.spend.export();
    if state
        .gateway_stores
        .spend
        .delete(&id, &actor, timestamp())
        .is_none()
    {
        return not_found(&id, request_id);
    }
    if let Err(message) = super::persist::save(&state).await {
        state.gateway_stores.spend.replace(previous);
        tracing::error!(%message);
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "failed to persist spend limit deletion",
            request_id,
        );
    }
    success(
        StatusCode::OK,
        &DeletedResponse {
            id,
            object_type: "spend_limit_deleted",
        },
        request_id,
    )
}

pub async fn method_not_allowed() -> Response {
    let request_id = request_id();
    error(
        StatusCode::METHOD_NOT_ALLOWED,
        "invalid_request_error",
        "method not allowed",
        request_id,
    )
}

fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    write_required: bool,
    request_id: &str,
) -> Result<String, Box<Response>> {
    let Some(admin) = state
        .config
        .server
        .gateway
        .as_ref()
        .and_then(|gateway| gateway.admin.as_ref())
    else {
        return Err(Box::new(error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid x-api-key",
            request_id.to_string(),
        )));
    };
    let presented = headers.get("x-api-key").map(|value| value.as_bytes());
    let mut matched: Option<(Access, &str)> = None;
    if let Some(presented) = presented {
        for (id, key) in &admin.read_keys {
            if constant_time_eq(presented, key.as_bytes()) {
                matched = Some((Access::Read, id));
            }
        }
        for (id, key) in &admin.write_keys {
            if constant_time_eq(presented, key.as_bytes()) {
                matched = Some((Access::Write, id));
            }
        }
    }
    match matched {
        Some((Access::Read, _)) if write_required => Err(Box::new(error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "read-only admin key cannot mutate spend limits",
            request_id.to_string(),
        ))),
        Some((_, id)) => Ok(format!("admin-key:{id}")),
        None => Err(Box::new(error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid x-api-key",
            request_id.to_string(),
        ))),
    }
}

fn paginate(limits: Vec<SpendLimit>, query: &ListQuery) -> Result<ListResponse, String> {
    let (start, end, has_more) = if let Some(after) = &query.after_id {
        let cursor = limits
            .iter()
            .position(|limit| &limit.id == after)
            .ok_or_else(|| format!("unknown after_id {after:?}"))?;
        let start = cursor + 1;
        let end = (start + query.limit).min(limits.len());
        (start, end, end < limits.len())
    } else if let Some(before) = &query.before_id {
        let cursor = limits
            .iter()
            .position(|limit| &limit.id == before)
            .ok_or_else(|| format!("unknown before_id {before:?}"))?;
        let start = cursor.saturating_sub(query.limit);
        (start, cursor, start > 0)
    } else {
        let end = query.limit.min(limits.len());
        (0, end, end < limits.len())
    };
    let data = limits[start..end].to_vec();
    Ok(ListResponse {
        first_id: data.first().map(|limit| limit.id.clone()),
        last_id: data.last().map(|limit| limit.id.clone()),
        data,
        has_more,
    })
}

#[derive(Debug)]
enum ScopeError {
    Unsupported(String),
    Invalid(String),
}

fn parse_scope(value: serde_json::Value) -> Result<Scope, ScopeError> {
    let object = value
        .as_object()
        .ok_or_else(|| ScopeError::Invalid("scope must be an object".into()))?;
    let scope_type = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ScopeError::Invalid("scope.type must be a string".into()))?;
    match scope_type {
        "organization" => Ok(Scope::Organization),
        "user" => {
            let user_id = object
                .get("user_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ScopeError::Invalid("user scope requires user_id".into()))?;
            let scope = Scope::User {
                user_id: user_id.to_string(),
            };
            validate_scope(&scope).map_err(|_| {
                ScopeError::Invalid(format!(
                    "user scope requires a non-empty user_id of at most {MAX_USER_ID_LENGTH} bytes"
                ))
            })?;
            Ok(scope)
        }
        "rbac_group" | "seat_tier" | "organization_service" => {
            Err(ScopeError::Unsupported(scope_type.to_string()))
        }
        _ => Err(ScopeError::Invalid(format!(
            "invalid scope type {scope_type:?}"
        ))),
    }
}

fn unsupported_scope(scope_type: &str, request_id: String) -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        format!("the gateway does not support scope type {scope_type:?} yet"),
        request_id,
    )
}

fn not_found(id: &str, request_id: String) -> Response {
    error(
        StatusCode::NOT_FOUND,
        "not_found_error",
        format!("spend limit {id:?} not found"),
        request_id,
    )
}

fn success<T: Serialize>(status: StatusCode, value: &T, request_id: String) -> Response {
    let mut response = (status, Json(value)).into_response();
    response.headers_mut().insert(
        "request-id",
        HeaderValue::from_str(&request_id).expect("generated request id is a valid header"),
    );
    response
}

fn error(
    status: StatusCode,
    error_type: &'static str,
    message: impl Into<String>,
    request_id: String,
) -> Response {
    let body = ErrorResponse {
        object_type: "error",
        error: ErrorDetail {
            error_type,
            message: message.into(),
        },
        request_id: request_id.clone(),
    };
    success(status, &body, request_id)
}

fn request_id() -> String {
    format!("req_{}", crate::admin::session::random_id())
}

fn timestamp() -> String {
    crate::auth::shared::format_iso8601_millis(SystemTime::now())
}
