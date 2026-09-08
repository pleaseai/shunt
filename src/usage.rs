//! Client-facing read-only pool usage endpoint (`GET /usage`), opt-in via
//! `[server.usage]`.
//!
//! Exposes a **sanitized, aggregated** view of the shared account pool's quota
//! state — per-window remaining headroom and reset time — so a non-admin client
//! (a `[server.auth]` token holder) can anticipate throttling without the admin
//! surface. Unlike `GET /admin/pool`, it never reveals account identities,
//! counts, priorities, disabled flags, thresholds, or burn-rate headroom: the
//! response carries only aggregate numbers derived across the pool, plus the
//! same aggregate computed per pooled provider (keyed by the provider's config
//! name) so a client that routes to one provider can read that provider's
//! headroom instead of the blended pool-wide figure.
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

use std::collections::{hash_map::Entry, BTreeMap, HashMap, HashSet};

use crate::{
    accounts::{account_key, AccountKey, AccountSnapshot},
    auth::claude::store as claude_store,
    config::{AccountConfig, AuthMode},
    error::ShuntError,
    server::AppState,
};

/// Sanitized aggregate returned by `GET /usage`.
#[derive(Debug, Serialize, PartialEq)]
pub struct UsageResponse {
    /// Aggregate across every pooled provider (the original, pool-wide view).
    pub pool: PoolStatus,
    /// The same aggregate scoped to each pooled provider, keyed by the
    /// configured provider name. Providers whose auth mode is not pooled are
    /// omitted, exactly as they are from `pool`. Empty when no provider pools.
    pub providers: BTreeMap<String, PoolStatus>,
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
    /// `mean(1 - utilization)` over non-disabled accounts reporting this
    /// window — the fraction of the pool's combined capacity still unused,
    /// clamped to `0.0..=1.0` and rounded to four decimals. Nine exhausted
    /// accounts plus one fresh one report `0.1`, not `1.0`. This is a
    /// pool-wide aggregate, not a prediction of whether the next request will
    /// be admitted (routing also weighs availability, model, session affinity,
    /// and priority). `None` when no non-disabled account reports the window.
    /// ChatGPT/Codex accounts populate the 5-hour and shared weekly windows
    /// from `x-codex-*` response headers; Codex has no Fable-scoped (`7d_oi`)
    /// signal, although another provider in a mixed pool may still supply
    /// that aggregate window.
    pub remaining: Option<f64>,
    /// Earliest reported reset time (unix epoch seconds) among the accounts
    /// counted in `remaining` — the soonest moment the aggregate can change.
    /// `None` when none of them reported one.
    pub resets_at: Option<u64>,
}

/// One pooled provider's snapshot rows, split the way the aggregates read
/// them: `status` is every configured row (each alias's own threshold verdict
/// counts), `pool_window` is the subset the pool-wide window means count
/// (aliases collapsed across providers, see [`representative_positions`]),
/// and `own_window` is the subset this provider's own entry counts (aliases
/// collapsed within the provider only, so an identity configured under two
/// providers still votes once in each provider's entry).
pub struct ProviderRows<'a> {
    pub name: &'a str,
    pub status: Vec<AccountSnapshot>,
    pub pool_window: Vec<AccountSnapshot>,
    pub own_window: Vec<AccountSnapshot>,
}

/// Collapse per-provider snapshots into the sanitized response: the pool-wide
/// aggregate over every provider's rows, plus one aggregate per provider over
/// that provider's rows only. Pure: the I/O (store scan) and locking happen in
/// the caller. Reads only aggregate numbers and availability booleans — no
/// account name, priority, `disabled` flag, threshold, or headroom leaves this
/// function; the only identifier emitted is the configured provider name.
pub fn aggregate_rows(providers: &[ProviderRows<'_>]) -> UsageResponse {
    let status_all = providers.iter().flat_map(|provider| provider.status.iter());
    let window_all = providers
        .iter()
        .flat_map(|provider| provider.pool_window.iter());
    UsageResponse {
        pool: pool_aggregate(status_all, window_all),
        providers: providers
            .iter()
            .map(|provider| {
                (
                    provider.name.to_string(),
                    pool_aggregate(provider.status.iter(), provider.own_window.iter()),
                )
            })
            .collect(),
    }
}

/// [`aggregate_rows`] for rows with no aliases to collapse: every row of every
/// provider counts for status and for both window means.
pub fn aggregate<N, S>(by_provider: &[(N, S)]) -> UsageResponse
where
    N: AsRef<str>,
    S: AsRef<[AccountSnapshot]>,
{
    let providers: Vec<ProviderRows<'_>> = by_provider
        .iter()
        .map(|(name, rows)| ProviderRows {
            name: name.as_ref(),
            status: rows.as_ref().to_vec(),
            pool_window: rows.as_ref().to_vec(),
            own_window: rows.as_ref().to_vec(),
        })
        .collect();
    aggregate_rows(&providers)
}

/// The sanitized aggregate for one set of rows (the whole pool, or one
/// provider's slice of it): `status` reads every row, the window means read
/// only the representative subset.
fn pool_aggregate<'a>(
    status_snapshots: impl Iterator<Item = &'a AccountSnapshot>,
    window_snapshots: impl Iterator<Item = &'a AccountSnapshot> + Clone,
) -> PoolStatus {
    PoolStatus {
        status: pool_status(status_snapshots),
        windows: Windows {
            five_hour: window_status(
                window_snapshots.clone(),
                |s| s.utilization_5h,
                |s| s.reset_5h,
            ),
            seven_day: window_status(
                window_snapshots.clone(),
                |s| s.utilization_7d,
                |s| s.reset_7d,
            ),
            fable: window_status(window_snapshots, |s| s.utilization_7d_oi, |s| s.reset_7d_oi),
        },
    }
}

/// Aggregate headroom for one window: the mean `1 - utilization` over the
/// non-disabled accounts reporting a finite utilization for it (the fraction of
/// the pool's combined capacity still unused), and the earliest reset any of
/// them reported. Not a guarantee about which account the next request will
/// actually route to.
fn window_status<'a>(
    snapshots: impl Iterator<Item = &'a AccountSnapshot>,
    utilization: impl Fn(&AccountSnapshot) -> Option<f64>,
    reset: impl Fn(&AccountSnapshot) -> Option<u64>,
) -> WindowStatus {
    let mut reporting = 0usize;
    let mut headroom_sum = 0.0;
    let mut earliest_reset: Option<u64> = None;
    for snapshot in snapshots.filter(|snapshot| !snapshot.disabled) {
        let Some(used) = utilization(snapshot).filter(|used| used.is_finite()) else {
            continue;
        };
        reporting += 1;
        headroom_sum += (1.0 - used).clamp(0.0, 1.0);
        if let Some(at) = reset(snapshot) {
            earliest_reset = Some(earliest_reset.unwrap_or(at).min(at));
        }
    }
    if reporting == 0 {
        return WindowStatus {
            remaining: None,
            resets_at: None,
        };
    }
    WindowStatus {
        remaining: Some(round4(headroom_sum / reporting as f64)),
        resets_at: earliest_reset,
    }
}

/// Pick one representative per physical account across every provider, and
/// return the chosen entries' positions per provider (both indexed like
/// `resolved`; `AccountPool::snapshot` emits one row per entry in input order,
/// so a position identifies a row where a name may not — a scoped store entry
/// and an inline entry can share a name). Config
/// entries that alias one account (same `account_key`, e.g. two names sharing
/// a `uuid`) are one account to the pool (`collapse_representatives`), yet
/// `AccountPool::snapshot` emits one identical quota row per entry, so the mean
/// would otherwise give that subscription one vote per alias. The key carries
/// the store family and stable identity but not the provider name, so the same
/// identity configured under two providers is one account too — hence one pass
/// over all providers. An enabled alias wins over a disabled one so a disabled
/// first alias does not hide an identity that still serves; order is otherwise
/// first-seen. Only the window aggregates use this subset — `status` keeps
/// reading every row, so each alias's own threshold verdict still counts.
fn representative_positions(resolved: &[(&str, Vec<AccountConfig>)]) -> Vec<HashSet<usize>> {
    let mut by_key: HashMap<AccountKey, (usize, usize)> = HashMap::new();
    // One slot per input entry, so a slot index *is* the entry's position.
    let mut chosen: Vec<Vec<Option<&AccountConfig>>> = Vec::with_capacity(resolved.len());
    for (provider_index, (provider, accounts)) in resolved.iter().enumerate() {
        let mut kept: Vec<Option<&AccountConfig>> = Vec::with_capacity(accounts.len());
        for (position, account) in accounts.iter().enumerate() {
            let mut keep = false;
            match by_key.entry(account_key(provider, account)) {
                Entry::Occupied(mut entry) => {
                    let (seen_provider, seen_index) = *entry.get();
                    let seen_disabled = if seen_provider == provider_index {
                        kept[seen_index].is_some_and(|seen| seen.disabled)
                    } else {
                        chosen[seen_provider][seen_index].is_some_and(|seen| seen.disabled)
                    };
                    if seen_disabled && !account.disabled {
                        if seen_provider == provider_index {
                            kept[seen_index] = None;
                        } else {
                            chosen[seen_provider][seen_index] = None;
                        }
                        entry.insert((provider_index, position));
                        keep = true;
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert((provider_index, position));
                    keep = true;
                }
            }
            kept.push(keep.then_some(account));
        }
        chosen.push(kept);
    }
    chosen
        .into_iter()
        .map(|kept| {
            kept.into_iter()
                .enumerate()
                .filter_map(|(position, account)| account.map(|_| position))
                .collect()
        })
        .collect()
}

/// Coarse pool health derived purely from availability booleans (no numbers):
/// `exhausted` when every selectable account is unavailable, `degraded` when any
/// is near quota, else `ok`. Disabled accounts never count as selectable.
fn pool_status<'a>(snapshots: impl Iterator<Item = &'a AccountSnapshot>) -> &'static str {
    let mut any_selectable = false;
    let mut any_available = false;
    let mut any_near_quota = false;

    for snapshot in snapshots.filter(|snapshot| !snapshot.disabled) {
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

    let mut resolved_by_provider: Vec<(&str, Vec<AccountConfig>)> = Vec::new();
    for (name, provider) in &state.config.providers {
        if !matches!(
            provider.auth,
            AuthMode::ClaudeOauth | AuthMode::ChatgptOauth | AuthMode::KimiOauth
        ) {
            continue;
        }
        let resolved = match provider.auth {
            AuthMode::ClaudeOauth => {
                crate::auth::shared::resolve_pool_accounts(
                    "Claude",
                    &provider.accounts,
                    &provider.account_scope,
                    crate::accounts::StoreFamily::Claude,
                    claude_store::default_accounts_dir(),
                    claude_store::scan_accounts,
                )
                .await
            }
            AuthMode::ChatgptOauth => {
                crate::auth::shared::resolve_pool_accounts(
                    "codex",
                    &provider.accounts,
                    &provider.account_scope,
                    crate::accounts::StoreFamily::Chatgpt,
                    crate::auth::codex::store::default_accounts_dir(),
                    crate::auth::codex::store::scan_accounts,
                )
                .await
            }
            AuthMode::KimiOauth => {
                crate::auth::shared::resolve_pool_accounts(
                    "Kimi",
                    &provider.accounts,
                    &provider.account_scope,
                    crate::accounts::StoreFamily::Kimi,
                    crate::auth::kimi::store::default_accounts_dir(),
                    crate::auth::kimi::store::scan_accounts,
                )
                .await
            }
            _ => unreachable!("provider auth filtered above"),
        };
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::error!(provider = %name, %error, "usage: failed to resolve account scope");
                return ShuntError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "failed to read pool usage",
                )
                .into_response();
            }
        };
        resolved_by_provider.push((name.as_str(), resolved));
    }
    // Aliases collapse across providers for the pool-wide means, and within
    // each provider for that provider's own entry.
    let pool_representatives = representative_positions(&resolved_by_provider);
    let mut providers: Vec<ProviderRows<'_>> = Vec::with_capacity(resolved_by_provider.len());
    for (entry, pool_chosen) in resolved_by_provider.iter().zip(&pool_representatives) {
        let (name, accounts) = entry;
        let own_chosen = representative_positions(std::slice::from_ref(entry))
            .pop()
            .unwrap_or_default();
        let rows = state
            .accounts
            .snapshot(name, accounts, None, state.config.server.pool.as_ref());
        let select = |chosen: &HashSet<usize>| -> Vec<AccountSnapshot> {
            rows.iter()
                .enumerate()
                .filter(|(position, _)| chosen.contains(position))
                .map(|(_, snapshot)| snapshot.clone())
                .collect()
        };
        providers.push(ProviderRows {
            name,
            pool_window: select(pool_chosen),
            own_window: select(&own_chosen),
            status: rows,
        });
    }
    Json(aggregate_rows(&providers)).into_response()
}

#[cfg(test)]
mod tests;
