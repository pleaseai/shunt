use axum::{extract::State, Json};
use serde::Serialize;

use crate::server::AppState;

#[derive(Debug, Serialize)]
pub struct RoutesResponse {
    pub data: Vec<RouteEntry>,
    /// The `[[models]]` entries carrying a `[models.router]` table.
    ///
    /// Omitted entirely when none is configured, so a deployment without a
    /// router serves the byte-identical response it served before routers
    /// existed — the two assertions in this module's tests pin exactly that.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routers: Vec<RouterEntry>,
}

/// One configured router, as `/routes` reports it.
///
/// The tunables (`confidence_threshold`, dwell, TTL, weights) are deliberately
/// absent: this endpoint answers "where can a request go", and a client that
/// needs the calibration reads the config. What it does report is every id the
/// router chooses between, because nothing else in this response is required
/// to name them: a router's targets need not have `[[routes]]` entries of
/// their own, though one that does still appears in `data` as itself.
///
/// The three stage fields stay `Option` with `skip_serializing_if` so a
/// `random` or `noop` entry does not report a tier pair it does not have —
/// a reader that finds `capable_target` present knows it is looking at a
/// two-tier algorithm.
#[derive(Debug, Serialize)]
pub struct RouterEntry {
    /// The advertised id clients request.
    pub model: String,
    /// The `[models.router] type` that decides this id.
    pub algorithm: String,
    /// Every target the router can name, in declared order. `[capable,
    /// efficient]` for a stage or auto entry, the configured list for a
    /// `random` one, and empty for `noop`, which answers as itself.
    pub targets: Vec<String>,
    /// Every id this router *consults* and never serves — today the
    /// `[models.router.classifier]` judge (ADR-0005 §7). Separate from
    /// `targets` because a reader resolving "where can a request go" must not
    /// find a judge there: no client turn is ever routed to one. Omitted
    /// entirely when there is none, so an entry with no judge answers exactly
    /// as it did before the key existed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub judges: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capable_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub efficient_target: Option<String>,
    /// The tier `picker` falls back to when no signal decides a turn. A
    /// body-less caller resolving this id gets it for that reason, which is
    /// what makes it worth naming. It is not "where a session starts": a first
    /// turn that already carries decisive tool-result history is scored like
    /// any other and can land on the opposite tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_tier: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct RouteEntry {
    pub model: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Verbatim from `RouteConfig.service_tier` (the normalized config
    /// value, not the resolved-with-provider-fallback one from
    /// `routing::route_for`). An explicit `"default"` override now serializes
    /// as `"service_tier": "default"` rather than being omitted like an
    /// unset route -- config validation preserves the sentinel instead of
    /// collapsing it to `None` (see config::normalize_service_tier_value), so
    /// this discovery response can distinguish "explicitly disabled" from
    /// "never configured". That is intentional and informative, not a leak:
    /// the sentinel is still stripped before any upstream request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

/// Shunt-native endpoint exposing the configured `[[routes]]` table verbatim,
/// including any `claude-`/`anthropic-`-prefixed discovery aliases. Distinct
/// from `/v1/models`, which serves the narrower Anthropic-protocol
/// model-discovery response (only `id`/`display_name` from `[[models]]`).
pub async fn get(State(state): State<AppState>) -> Json<RoutesResponse> {
    // Snapshot the live config so this response reflects the latest reload.
    let state = state.refreshed();
    let response = snapshot(&state);
    tracing::info!(
        routes = response.data.len(),
        routers = response.routers.len(),
        "served GET /routes discovery"
    );
    Json(response)
}

/// Build the `/routes` payload from an already-refreshed state.
///
/// Shared with the admin surface's `GET /admin/api/routes`, which serves this
/// same view behind admin authentication. The duplication is deliberate rather
/// than a redirect: this route is discovery, deliberately unauthenticated so any
/// client can resolve a model against it, while everything under `/admin` is
/// gated by the admin credential. Pointing one at the other would tie the two
/// namespaces' authentication together -- either widening what the admin
/// credential gates or putting an auth challenge in front of discovery. Both
/// callers share this function so the table an operator reads cannot drift from
/// the one a client resolves against.
pub(crate) fn snapshot(state: &AppState) -> RoutesResponse {
    let data: Vec<RouteEntry> = state
        .config
        .routes
        .iter()
        .map(|route| RouteEntry {
            model: route.model.clone(),
            provider: route.provider.clone(),
            upstream_model: route.upstream_model.clone(),
            effort: route.effort.clone(),
            service_tier: route.service_tier.clone(),
        })
        .collect();
    let routers: Vec<RouterEntry> = state
        .config
        .models
        .iter()
        .filter_map(|model| {
            let router = model.router.as_ref()?;
            let stage = router.stage();
            Some(RouterEntry {
                model: model.id.clone(),
                algorithm: router.algorithm().to_string(),
                targets: deduplicated(router.targets()),
                judges: deduplicated(router.judges()),
                capable_target: stage.map(|stage| stage.capable_target.clone()),
                efficient_target: stage.map(|stage| stage.efficient_target.clone()),
                default_tier: stage.map(|stage| match stage.picker {
                    crate::config::StageRouterPicker::EfficientFirst => "efficient",
                    crate::config::StageRouterPicker::CapableFirst => "capable",
                }),
            })
        })
        .collect();
    RoutesResponse { data, routers }
}

/// The ids in declared order, with a repeat dropped.
///
/// A custom `llm_classifier` names the same model id under several groups —
/// upstream's own example lists every answer model in `any` *and* in the group
/// that selects it — so the raw enumeration repeats. `/routes` answers "where
/// can a request go", and the same destination twice is not two places.
/// Order is the declared one rather than sorted, because that is the order the
/// operator wrote and the one every other algorithm's list already uses.
fn deduplicated(ids: Vec<&str>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::with_capacity(ids.len());
    for id in ids {
        if !seen.iter().any(|kept| kept == id) {
            seen.push(id.to_string());
        }
    }
    seen
}

#[cfg(test)]
mod tests;
