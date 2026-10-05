//! `GET /v1/organizations/spend_limits/effective`: one row per principal and
//! period with the cap that binds it and its period-to-date spend.
//!
//! shunt has no identity store, so a row's `actor` is derived from the
//! principal string alone (see [`actor`]).

use axum::{
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use url::form_urlencoded;

use super::{
    api::{authenticate, error, request_id, success},
    meter::{effective_limit, now_secs, ANONYMOUS_PRINCIPAL, FEMTO_USD_PER_CENT, PERIODS},
    store::{Period, Scope, SpendLimit},
};
use crate::{config::AdminAccess, server::AppState};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 1000;
const MAX_USER_IDS: usize = 100;
const MAX_QUERY_LENGTH: usize = 256;

#[derive(Debug, Serialize)]
struct EffectiveResponse {
    data: Vec<SpendSummary>,
    next_page: Option<String>,
}

#[derive(Debug, Serialize)]
struct SpendSummary {
    scope: Scope,
    groups: Vec<String>,
    actor: Actor,
    amount: Option<String>,
    currency: &'static str,
    period: Period,
    source: Option<Scope>,
    spend_limit_id: Option<String>,
    period_to_date_spend: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct Actor {
    #[serde(rename = "type")]
    actor_type: &'static str,
    user_id: String,
    name: Option<String>,
    email_address: Option<String>,
    deleted: bool,
}

/// What the client sent, after validation.
struct Params {
    limit: usize,
    periods: Vec<Period>,
    user_ids: Option<Vec<String>>,
    /// The `q` filter, lowercased once at parse time.
    q: Option<String>,
    spend_desc: bool,
    page: Option<Cursor>,
}

/// The opaque `next_page` token: base64url of this JSON.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    p: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    c: Option<u64>,
}

fn bad_request(message: impl Into<String>, request_id: String) -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        message,
        request_id,
    )
}

pub async fn effective(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let request_id = request_id();
    let state = state.refreshed();
    if let Err(response) = authenticate(&state, &headers, AdminAccess::Read, &request_id) {
        return *response;
    }
    let params = match parse_params(query.as_deref().unwrap_or("")) {
        Ok(params) => params,
        Err(message) => return bad_request(message, request_id),
    };
    let store = &state.gateway_stores.spend;
    let limits = store.list();
    let now = now_secs();
    let meter = store.meter();

    let (principals, next_page) = match &params.user_ids {
        Some(ids) => (
            ids.iter()
                .filter(|id| matches_query(id, params.q.as_deref()))
                .cloned()
                .collect(),
            None,
        ),
        None => {
            let candidates: Vec<String> = meter
                .principals()
                .into_iter()
                .filter(|id| matches_query(id, params.q.as_deref()))
                .collect();
            select_page(candidates, &params, |principal| {
                meter.spent(principal, params.periods[0], now)
            })
        }
    };
    let static_token = |name: &str| {
        state
            .inbound_auth
            .as_ref()
            .is_some_and(|auth| auth.has_token_name(name))
    };
    let mut data = Vec::with_capacity(principals.len() * params.periods.len());
    for principal in &principals {
        for &period in &params.periods {
            let cap = effective_limit(&limits, principal, period);
            data.push(summary(
                principal,
                period,
                cap,
                meter.spent(principal, period, now),
                &static_token,
            ));
        }
    }
    success(
        StatusCode::OK,
        &EffectiveResponse { data, next_page },
        request_id,
    )
}

fn summary(
    principal: &str,
    period: Period,
    cap: Option<&SpendLimit>,
    spent_femto: u64,
    static_token: &dyn Fn(&str) -> bool,
) -> SpendSummary {
    SpendSummary {
        scope: Scope::User {
            user_id: principal.to_string(),
        },
        groups: Vec::new(),
        actor: actor(principal, static_token),
        amount: cap.and_then(|limit| limit.amount.clone()),
        currency: "USD",
        period,
        source: cap.map(|limit| limit.scope.clone()),
        spend_limit_id: cap.map(|limit| limit.id.clone()),
        period_to_date_spend: format_cents(spent_femto),
    }
}

/// shunt has no user directory, so the display fields come from the principal:
/// a configured static token's name is a name, the reserved anonymous principal
/// has neither, and anything else is an email only when it looks like one (a
/// JWT `sub` is often opaque and gets a null `email_address`).
fn actor(principal: &str, static_token: &dyn Fn(&str) -> bool) -> Actor {
    let (name, email_address) = if principal == ANONYMOUS_PRINCIPAL {
        (None, None)
    } else if static_token(principal) {
        (Some(principal.to_string()), None)
    } else if looks_like_email(principal) {
        (None, Some(principal.to_string()))
    } else {
        // An opaque identity (a JWT `sub`, say) is a user id, not an email.
        (None, None)
    };
    Actor {
        actor_type: "user_actor",
        user_id: principal.to_string(),
        name,
        email_address,
        deleted: false,
    }
}

/// Exactly one `@`, a non-empty local part, and a domain containing a `.`.
fn looks_like_email(principal: &str) -> bool {
    let mut parts = principal.split('@');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(local), Some(domain), None) if !local.is_empty() && domain.contains('.')
    )
}

/// Femto-USD as US cents with up to three decimals (half-up), trailing zeros
/// and a trailing `.` stripped. Integer arithmetic only.
fn format_cents(femto: u64) -> String {
    let per_milli_cent = FEMTO_USD_PER_CENT / 1000;
    let milli = (u128::from(femto) + per_milli_cent / 2) / per_milli_cent;
    let (whole, frac) = (milli / 1000, milli % 1000);
    if frac == 0 {
        return whole.to_string();
    }
    format!("{whole}.{frac:03}")
        .trim_end_matches('0')
        .to_string()
}

/// `q` is already lowercased (see [`Params::q`]).
fn matches_query(principal: &str, q: Option<&str>) -> bool {
    // The derived name and email are the principal itself, so one haystack.
    q.is_none_or(|q| principal.to_lowercase().contains(q))
}

/// Orders `candidates`, applies the cursor and takes one page. Returns the
/// page's principals and the token for the next one.
fn select_page(
    candidates: Vec<String>,
    params: &Params,
    spent: impl Fn(&str) -> u64,
) -> (Vec<String>, Option<String>) {
    // Each entry carries its spend so the cursor and the order share one read.
    // Candidates arrive ascending, which is already the default order.
    let mut entries: Vec<(String, u64)> = candidates
        .into_iter()
        .map(|principal| {
            let value = if params.spend_desc {
                spent(&principal)
            } else {
                0
            };
            (principal, value)
        })
        .collect();
    if params.spend_desc {
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }
    if let Some(cursor) = &params.page {
        entries.retain(|(principal, value)| match cursor.c {
            Some(after) => {
                (*value, std::cmp::Reverse(principal)) < (after, std::cmp::Reverse(&cursor.p))
            }
            None => *principal > cursor.p,
        });
    }
    let more = entries.len() > params.limit;
    entries.truncate(params.limit);
    let next_page = if more {
        entries.last().map(|(principal, value)| {
            encode(&Cursor {
                p: principal.clone(),
                c: params.spend_desc.then_some(*value),
            })
        })
    } else {
        None
    };
    (
        entries
            .into_iter()
            .map(|(principal, _)| principal)
            .collect(),
        next_page,
    )
}

fn encode(cursor: &Cursor) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor).expect("cursor serializes"))
}

fn decode(token: &str, spend_desc: bool) -> Option<Cursor> {
    let bytes = URL_SAFE_NO_PAD.decode(token).ok()?;
    let cursor: Cursor = serde_json::from_slice(&bytes).ok()?;
    // A `spend_desc` cursor carries the spend it stopped at; a name-order one
    // does not.
    (cursor.c.is_some() == spend_desc).then_some(cursor)
}

/// Validates in the reference gateway's order, reporting the first failure.
fn parse_params(raw: &str) -> Result<Params, String> {
    let mut limit = None;
    let mut periods: Vec<String> = Vec::new();
    let mut user_ids: Option<Vec<String>> = None;
    let (mut q, mut sort, mut page) = (None, None, None);
    for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
        let value = value.into_owned();
        match key.as_ref() {
            "limit" => limit = limit.or(Some(value)),
            "period[]" => periods.push(value),
            "user_ids[]" => user_ids.get_or_insert_with(Vec::new).push(value),
            "q" => q = q.or(Some(value)),
            "sort" => sort = sort.or(Some(value)),
            "page" => page = page.or(Some(value)),
            _ => {}
        }
    }

    let limit = match limit {
        None => DEFAULT_LIMIT,
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=MAX_LIMIT).contains(limit))
            .ok_or("limit: must be between 1 and 1000")?,
    };

    let mut parsed_periods = Vec::new();
    for value in &periods {
        let period = match value.as_str() {
            "daily" => Period::Daily,
            "weekly" => Period::Weekly,
            "monthly" => Period::Monthly,
            _ => return Err("period[]: must be one of daily, weekly, monthly".into()),
        };
        if !parsed_periods.contains(&period) {
            parsed_periods.push(period);
        }
    }
    if parsed_periods.is_empty() {
        parsed_periods = PERIODS.to_vec();
    }

    if user_ids
        .as_ref()
        .is_some_and(|ids| ids.len() > MAX_USER_IDS)
    {
        return Err("user_ids[]: at most 100 entries per request".into());
    }
    // Repeats answer once, at the first-seen position.
    let user_ids = user_ids.map(|ids| {
        let mut unique: Vec<String> = Vec::with_capacity(ids.len());
        for id in ids {
            if !unique.contains(&id) {
                unique.push(id);
            }
        }
        unique
    });
    if q.as_ref()
        .is_some_and(|q| q.chars().count() > MAX_QUERY_LENGTH)
    {
        return Err("q: too long".into());
    }
    let q = q.map(|q| q.to_lowercase());

    let spend_desc = match sort.as_deref() {
        None => false,
        Some("spend_desc") => true,
        Some(_) => return Err("sort: must be spend_desc".into()),
    };
    if spend_desc && parsed_periods.len() != 1 {
        return Err("sort=spend_desc requires exactly one period[]".into());
    }

    let page = match page {
        None => None,
        Some(token) => Some(decode(&token, spend_desc).ok_or("page: invalid page token")?),
    };
    Ok(Params {
        limit,
        periods: parsed_periods,
        user_ids,
        q,
        spend_desc,
        page,
    })
}

#[cfg(test)]
mod tests {
    use super::format_cents;

    #[test]
    fn format_cents_trims_zeros_and_rounds_half_up_to_three_decimals() {
        let cent = 10_000_000_000_000_u64;
        assert_eq!(format_cents(0), "0");
        assert_eq!(format_cents(cent / 2 + 12 * cent), "12.5");
        assert_eq!(format_cents(1234 * cent), "1234");
        assert_eq!(format_cents(123 * cent + 456 * (cent / 1000)), "123.456");
        // Half a thousandth of a cent rounds up; just under it rounds down.
        assert_eq!(format_cents(cent / 1000 / 2), "0.001");
        assert_eq!(format_cents(cent / 1000 / 2 - 1), "0");
        assert_eq!(format_cents(u64::MAX), "1844674.407");
    }
}
