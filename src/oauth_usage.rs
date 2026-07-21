//! Inbound `GET /api/oauth/usage` synthesizer, opt-in via `[server.oauth_usage]`.
//!
//! Claude Code's own CLI renders its native usage bars (`Current session`,
//! `Current week (all models)`, and — gated by a server-side allowlist —
//! `Current week (Fable)`) by calling `GET {ANTHROPIC_BASE_URL}/api/oauth/usage`
//! itself (`fetchUtilization`). When `ANTHROPIC_BASE_URL` points at shunt
//! instead of `https://api.anthropic.com`, that path 404s today — shunt
//! registers no such route — so the CLI's own bars silently render empty. This
//! module reshapes accounts-pool telemetry into the exact wire format the CLI
//! expects, mirroring [`crate::auth::claude::usage`] (the outbound client for
//! the same upstream endpoint) in the opposite direction.
//!
//! See `docs/m14-oauth-usage-endpoint.md` for the full contract, including the
//! documented precondition (which CLI login modes actually trigger the fetch)
//! and the auth model (bind-topology-gated, not credential-matched — the
//! bearer the CLI presents here is its own Anthropic OAuth session token, not
//! a shunt client token).

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::{
    accounts::AccountSnapshot, auth::claude::store as claude_store,
    auth::claude::usage::FABLE_SCOPE_DISPLAY_NAME, auth::shared::format_iso8601, config::AuthMode,
    error::ShuntError, server::AppState,
};

/// Exactly Anthropic's `/api/oauth/usage` schema, as parsed by
/// `crate::auth::claude::usage`'s `parse_usage`/`parse_window`/
/// `parse_fable_window` — this is not a new contract, it is the mirror of one
/// shunt already consumes.
#[derive(Debug, Default, Serialize, PartialEq)]
pub(crate) struct OauthUsageWire {
    #[serde(skip_serializing_if = "Option::is_none")]
    five_hour: Option<WindowWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seven_day: Option<WindowWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    limits: Vec<LimitWire>,
}

#[derive(Debug, Serialize, PartialEq)]
struct WindowWire {
    utilization: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct LimitWire {
    kind: &'static str,
    scope: LimitScope,
    percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct LimitScope {
    model: LimitScopeModel,
}

#[derive(Debug, Serialize, PartialEq)]
struct LimitScopeModel {
    display_name: &'static str,
}

/// Round a fraction*100 percent to two decimals so the response does not echo
/// raw float noise.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Collapse one window's `(utilization fraction, resets_at epoch)` into the
/// wire shape's `(percent, RFC3339 resets_at)`. Clamping defensively even
/// though `note_quota`/`note_codex_quota` already write fractions in
/// `0.0..=1.0` in practice — this is a response-shape boundary, not an
/// internal invariant.
fn to_percent_and_reset(utilization: f64, reset: Option<u64>) -> (f64, Option<String>) {
    let percent = round2(utilization.clamp(0.0, 1.0) * 100.0);
    let resets_at = reset
        .map(|secs| format_iso8601(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)));
    (percent, resets_at)
}

/// Routing-aware, priority-tiered worst case for one window: the
/// deliberately coarser (but not misleading) alternative to a pool-wide
/// least-utilized aggregate — see `docs/m14-oauth-usage-endpoint.md`,
/// "Aggregation policy", for the full rationale.
///
/// 1. Filter to non-disabled accounts reporting a finite utilization for this
///    window.
/// 2. Prefer the `available` subset (not disabled, not cooling, not near
///    quota); fall back to the full filtered set when none are available —
///    mirrors `select_order`'s real behavior of still routing to a
///    near-quota/cooling account when nothing else is left.
/// 3. Within whichever set step 2 selected, take the accounts at the lowest
///    `priority` value present (the most-preferred tier `select_order` tries
///    first).
/// 4. Within that tier, report the maximum utilization (worst case) — the
///    tier is exactly the set of accounts round-robin/sticky selection can
///    hit for the *next* request.
fn routing_aware_window(
    snapshots: &[AccountSnapshot],
    utilization: impl Fn(&AccountSnapshot) -> Option<f64>,
    reset: impl Fn(&AccountSnapshot) -> Option<u64>,
) -> Option<(f64, Option<u64>)> {
    let candidates: Vec<&AccountSnapshot> = snapshots
        .iter()
        .filter(|s| !s.disabled)
        .filter(|s| utilization(s).is_some_and(f64::is_finite))
        .collect();
    let usable: Vec<&AccountSnapshot> =
        candidates.iter().copied().filter(|s| s.available).collect();
    let pool = if usable.is_empty() {
        candidates
    } else {
        usable
    };
    let min_priority = pool.iter().map(|s| s.priority).min()?;
    pool.into_iter()
        .filter(|s| s.priority == min_priority)
        .map(|s| (utilization(s).expect("filtered above"), reset(s)))
        .max_by(|(a, _), (b, _)| a.total_cmp(b))
}

/// Build a window entry from `routing_aware_window`'s result, or `None` when
/// no non-disabled Claude account reports the window (omitted from the
/// response rather than a fabricated `0%` — matches M12's "no signal"
/// convention).
fn window_wire(
    snapshots: &[AccountSnapshot],
    utilization: impl Fn(&AccountSnapshot) -> Option<f64>,
    reset: impl Fn(&AccountSnapshot) -> Option<u64>,
) -> Option<WindowWire> {
    let (used, resets_at) = routing_aware_window(snapshots, utilization, reset)?;
    let (percent, resets_at) = to_percent_and_reset(used, resets_at);
    Some(WindowWire {
        utilization: percent,
        resets_at,
    })
}

/// Collapse Claude-account snapshots into the CLI-facing wire shape. Pure: no
/// I/O, no locking — the caller resolves the snapshots first. `pub(crate)` so
/// `crate::auth::claude::usage`'s test module can round-trip a built response
/// through its own private parser (see that module's tests).
pub(crate) fn to_wire(snapshots: &[AccountSnapshot]) -> OauthUsageWire {
    let five_hour = window_wire(snapshots, |s| s.utilization_5h, |s| s.reset_5h);
    let seven_day = window_wire(snapshots, |s| s.utilization_7d, |s| s.reset_7d);
    let fable = routing_aware_window(snapshots, |s| s.utilization_7d_oi, |s| s.reset_7d_oi);
    let limits = match fable {
        Some((used, resets_at)) => {
            let (percent, resets_at) = to_percent_and_reset(used, resets_at);
            vec![LimitWire {
                kind: "weekly_scoped",
                scope: LimitScope {
                    model: LimitScopeModel {
                        display_name: FABLE_SCOPE_DISPLAY_NAME,
                    },
                },
                percent,
                resets_at,
            }]
        }
        None => Vec::new(),
    };
    OauthUsageWire {
        five_hour,
        seven_day,
        limits,
    }
}

pub async fn get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Snapshot the live config so this response reflects the latest reload
    // (matches discovery.rs / admin routes / usage::get).
    let state = state.refreshed();

    // `boot_is_loopback` is fixed at process startup (see `AppState` field
    // docs) — never re-derived from `state.config`, which a reload can
    // rewrite without moving the actual listener.
    if !state.boot_is_loopback {
        // Validate the caller like the gated Messages routes do
        // (`proxy::check_inbound_auth`): require a real client-token match or a
        // valid gateway JWT, never mere header presence. A non-loopback bind
        // always has one of `[server.auth]`/`[server.gateway]` configured (the
        // `OauthUsageEndpointRequiresAuthOnNonLoopback` config guard), and the
        // only CLI login mode that fetches `/api/oauth/usage` is a loopback
        // subscription login — so there is no unverifiable-Anthropic-bearer
        // caller to accommodate here, and accepting bare presence would let any
        // remote caller scrape pool quota telemetry with a fabricated header.
        let static_client = state
            .inbound_auth
            .as_ref()
            .and_then(|auth| auth.authenticate_client(&headers));
        let gateway_client = state
            .gateway_auth
            .as_ref()
            .and_then(|auth| auth.authenticate_bearer(&headers));
        if static_client.is_none() && gateway_client.is_none() {
            tracing::warn!(
                "inbound auth failed for GET /api/oauth/usage: missing or invalid credential on a non-loopback bind"
            );
            return ShuntError::new(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "missing or invalid credential: this gateway requires a valid client token or gateway login to read usage",
            )
            .into_response();
        }
        match static_client {
            Some(client) => {
                tracing::info!(client = %client, "inbound client authenticated for GET /api/oauth/usage")
            }
            None => {
                tracing::info!("inbound gateway login authenticated for GET /api/oauth/usage")
            }
        }
    }

    let mut snapshots = Vec::new();
    for (name, provider) in &state.config.providers {
        // Codex/Cursor/etc. never contribute to this endpoint: the CLI is
        // asking about its own Claude subscription, and blending in a
        // different backend's utilization would misreport it (see Deviation 1
        // in the milestone doc).
        if provider.auth != AuthMode::ClaudeOauth {
            continue;
        }
        // Resolve accounts through the shared pool resolver — exactly as the
        // sibling `usage::get` (M12) and the usage poller do — so this endpoint
        // reflects the same pool the router actually serves: it honors an
        // upstream's `account_scope` and merges store + inline accounts, rather
        // than the raw store scan an earlier revision used (which would include
        // scope-excluded accounts and drop scoped store accounts when inline
        // accounts are also configured).
        let resolved = match crate::auth::shared::resolve_pool_accounts(
            "Claude",
            &provider.accounts,
            &provider.account_scope,
            crate::accounts::StoreFamily::Claude,
            claude_store::default_accounts_dir(),
            claude_store::scan_accounts,
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::error!(provider = %name, %error, "oauth_usage: failed to resolve account scope");
                return ShuntError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "api_error",
                    "failed to read pool usage",
                )
                .into_response();
            }
        };
        snapshots.extend(state.accounts.snapshot(
            name,
            &resolved,
            None,
            state.config.server.pool.as_ref(),
        ));
    }
    Json(to_wire(&snapshots)).into_response()
}

#[cfg(test)]
mod tests;
