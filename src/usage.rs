//! Client-facing read-only pool usage endpoint (`GET /usage`), opt-in via
//! `[server.usage]`.
//!
//! Exposes a **sanitized, aggregated** view of the shared account pool's quota
//! state — per-window remaining headroom and reset time — so a non-admin client
//! (a `[server.auth]` token holder) can anticipate throttling without the admin
//! surface. Unlike `GET /admin/pool`, it never reveals account identities,
//! counts, priorities, disabled flags, thresholds, or burn-rate headroom: the
//! response carries only aggregate numbers derived across the pool.
//!
//! The endpoint requires `[server.auth]` (a non-admin caller must be
//! identifiable); the pairing is enforced at config validation, and the handler
//! fails closed if inbound auth is somehow absent.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::{
    accounts::AccountSnapshot, auth::claude::store as claude_store, config::AuthMode,
    error::ShuntError, server::AppState,
};

/// Sanitized aggregate returned by `GET /usage`.
#[derive(Debug, Serialize, PartialEq)]
pub struct UsageResponse {
    pub pool: PoolStatus,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct PoolStatus {
    /// Derived from account-availability booleans only (`ok` | `degraded` |
    /// `exhausted`); never carries a per-account number.
    pub status: &'static str,
    pub windows: Windows,
}

/// The three tracked rate-limit windows: the rolling 5-hour session window, the
/// shared weekly window, and the Fable-scoped weekly window (`7d_oi`).
#[derive(Debug, Serialize, PartialEq)]
pub struct Windows {
    #[serde(rename = "5h")]
    pub five_hour: WindowStatus,
    #[serde(rename = "7d")]
    pub seven_day: WindowStatus,
    pub fable: WindowStatus,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct WindowStatus {
    /// `1 - min(utilization)` over non-disabled accounts reporting this window
    /// — the least reported utilization among non-disabled accounts, clamped to
    /// `0.0..=1.0` and rounded to four decimals. This is a pool-wide aggregate,
    /// not a prediction of which account the next request will actually route
    /// to (routing also weighs availability, model, session affinity, and
    /// priority). `None` when no account reports the window (e.g. the Codex
    /// backend, which publishes no quota headers).
    pub remaining: Option<f64>,
    /// Reset time (unix epoch seconds) of the least-utilized account's window,
    /// when the backend reported one.
    pub resets_at: Option<u64>,
}

/// Collapse per-account snapshots into the sanitized pool aggregate. Pure: the
/// I/O (store scan) and locking happen in the caller. Reads only aggregate
/// numbers and availability booleans — no account name, priority, `disabled`
/// flag, threshold, or headroom leaves this function.
pub fn aggregate(snapshots: &[AccountSnapshot]) -> UsageResponse {
    UsageResponse {
        pool: PoolStatus {
            status: pool_status(snapshots),
            windows: Windows {
                five_hour: window_status(snapshots, |s| s.utilization_5h, |s| s.reset_5h),
                seven_day: window_status(snapshots, |s| s.utilization_7d, |s| s.reset_7d),
                fable: window_status(snapshots, |s| s.utilization_7d_oi, |s| s.reset_7d_oi),
            },
        },
    }
}

/// Aggregate headroom for one window: `1 - utilization` of the non-disabled
/// account reporting the least utilization for this window (and that
/// account's reset time), not a guarantee about which account the next
/// request will actually route to.
fn window_status(
    snapshots: &[AccountSnapshot],
    utilization: impl Fn(&AccountSnapshot) -> Option<f64>,
    reset: impl Fn(&AccountSnapshot) -> Option<u64>,
) -> WindowStatus {
    let least_utilized = snapshots
        .iter()
        .filter(|snapshot| !snapshot.disabled)
        .filter_map(|snapshot| {
            utilization(snapshot)
                .filter(|used| used.is_finite())
                .map(|used| (used, reset(snapshot)))
        })
        .min_by(|(a, _), (b, _)| a.total_cmp(b));
    match least_utilized {
        Some((used, resets_at)) => WindowStatus {
            remaining: Some(round4((1.0 - used).clamp(0.0, 1.0))),
            resets_at,
        },
        None => WindowStatus {
            remaining: None,
            resets_at: None,
        },
    }
}

/// Coarse pool health derived purely from availability booleans (no numbers):
/// `exhausted` when every selectable account is unavailable, `degraded` when any
/// is near quota, else `ok`. Disabled accounts never count as selectable.
fn pool_status(snapshots: &[AccountSnapshot]) -> &'static str {
    let mut any_selectable = false;
    let mut any_available = false;
    let mut any_near_quota = false;

    for snapshot in snapshots.iter().filter(|snapshot| !snapshot.disabled) {
        any_selectable = true;
        any_available |= snapshot.available;
        any_near_quota |= snapshot.near_quota;
    }

    if !any_selectable || !any_available {
        "exhausted"
    } else if any_near_quota {
        "degraded"
    } else {
        "ok"
    }
}

/// Round a fraction to four decimals so the response does not echo a raw f64
/// with float noise (and does not over-share account utilization precision).
fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

pub async fn get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Snapshot the live config so this response reflects the latest reload
    // (matches discovery.rs / admin routes).
    let state = state.refreshed();
    // `[server.usage]` requires `[server.auth]` at config validation, so inbound
    // auth is present in practice; fail closed rather than serve pool telemetry
    // unauthenticated if it somehow is not.
    let Some(auth) = state.inbound_auth.clone() else {
        return ShuntError::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "usage endpoint requires client authentication, but no client tokens are configured",
        )
        .into_response();
    };
    let Some(client) = auth.authenticate_client(&headers) else {
        tracing::warn!("inbound auth failed for GET /usage: missing or invalid client token");
        let message = format!(
            "missing or invalid credential: this gateway requires a client token (via {}, x-api-key, or Authorization: Bearer) to read pool usage; ask the operator for one",
            auth.header()
        );
        return ShuntError::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
            .into_response();
    };
    tracing::info!(client = %client, "inbound client authenticated for GET /usage");

    let config = state.config.clone();
    let accounts = state.accounts.clone();
    // scan_accounts does file I/O and snapshot locks a std mutex; run off the
    // async workers (mirrors admin::pool). Model is unset — the aggregate spans
    // every window regardless of any single request's model.
    let result = tokio::task::spawn_blocking(move || {
        let mut snapshots = Vec::new();
        for (name, provider) in &config.providers {
            if !matches!(
                provider.auth,
                AuthMode::ClaudeOauth | AuthMode::ChatgptOauth
            ) {
                continue;
            }
            let resolved = if provider.accounts.is_empty() {
                // Surface a store read failure as an error rather than an empty
                // pool: an I/O/permission problem must not masquerade as "no
                // accounts, full headroom".
                let scanned = match provider.auth {
                    AuthMode::ClaudeOauth => claude_store::scan_accounts(),
                    AuthMode::ChatgptOauth => crate::auth::codex::store::scan_accounts(),
                    _ => unreachable!("provider auth filtered above"),
                };
                match scanned {
                    Ok(list) => list,
                    Err(error) => {
                        tracing::error!(provider = %name, %error, "usage: failed to scan accounts store");
                        return Err(());
                    }
                }
            } else {
                provider.accounts.clone()
            };
            snapshots.extend(accounts.snapshot(
                name,
                &resolved,
                None,
                config.server.pool.as_ref(),
            ));
        }
        Ok(snapshots)
    })
    .await;

    match result {
        Ok(Ok(snapshots)) => Json(aggregate(&snapshots)).into_response(),
        Ok(Err(())) => ShuntError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "failed to read pool usage",
        )
        .into_response(),
        Err(join_error) => {
            tracing::error!(%join_error, "usage: pool snapshot task panicked");
            ShuntError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "failed to read pool usage",
            )
            .into_response()
        }
    }
}

#[cfg(test)]
mod tests;
