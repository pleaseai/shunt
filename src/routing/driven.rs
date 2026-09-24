//! The driven lane: `[models.router] type = "llm_classifier"` / `"composite"`
//! / `"advisor"` and the classifier form of `[models.subagents]` (ADR-0005 §8
//! PR 5, PR 6).
//!
//! Unlike the stage router's judge ([`crate::routing::judge`]), which shunt
//! drives a *bare* classifier for because the session state is shunt's, this
//! lane hands libsy the whole algorithm and keeps it. Affinity, retained
//! tiers, and the default-category close all live inside the instance, so the
//! instance must outlive the request: one per `[[models]]` entry, built in
//! [`RuntimeState::from_config`](crate::reload::RuntimeState::from_config)
//! beside `PrefillRouters` and rebuilt by a reload. The consequence is
//! deliberate and documented: a reload forgets every session's assignment, and
//! the next turn is classified again rather than replaying a decision made
//! against a table that is no longer configured.
//!
//! Because the algorithm's own fallback *is* the policy, shunt does not add
//! one. libsy folds every call failure — a `400`, a timeout, an unparseable
//! reply — into "no verdict", closes the cascade on
//! `DefaultCategoryClassifier`, and stamps the evidence that says so. It makes
//! exactly one `CallModel` with the whole judge list, and shunt's `serve`
//! dispatches to `call.models.first()` only, so a failed judge costs one call
//! and no more.
//!
//! # Evidence → [`RouteSource`]
//!
//! libsy reports *why* it chose a target in `OutcomeMetadata.evidence`, as a
//! `source` string. The map below is closed, and reading it is the only way
//! the header and the `shunt.router.decisions` label can distinguish a verdict
//! from a fallback — `selected_model_ids` alone cannot, because the fail-open
//! target and a deliberately chosen one are the same id.
//!
//! | `evidence.source` | written by | `[models.router]` / overlay | `type = "composite"` |
//! |---|---|---|---|
//! | `retained` | `util/affinity.rs`, `algorithms/composite.rs` | [`RouteSource::DrivenRetained`] | [`RouteSource::DrivenRetained`] |
//! | `fail_open` | `util/llm_judge.rs` (`report_fail_open`), `llm_class.rs` (`capability_evidence`, invalid verdict) | [`RouteSource::DrivenFailOpen`] | [`RouteSource::DrivenFailOpen`] |
//! | `fall_open` | `llm_class.rs` (`DefaultCategoryClassifier`), libsy's stage `FallOpen` | [`RouteSource::DrivenFailOpen`] | `Stage(tier, Scorer(FallOpen))` |
//! | `llm-classifier` | `llm_class.rs` (`capability_evidence`, valid verdict), libsy's stage `SourceStamp` | [`RouteSource::Driven`] | `Stage(tier, Scorer(LlmClassifier))` |
//! | `override` / `dimensions` / `ambiguous` / `capable_hold` | `util/stage.rs` | [`RouteSource::Driven`] | `Stage(tier, Scorer(…))` |
//! | absent, or anything else | — | [`RouteSource::Driven`] | [`RouteSource::Driven`] |
//!
//! Two strings that look alike are not: upstream spells the
//! `DefaultCategoryClassifier` close `fall_open` and the judge's own failure
//! `fail_open`. On a classifier entry both mean "no verdict decided this
//! turn", so both report `classifier_fail_open`; on a composite entry
//! `fall_open` is the *scorer's* picker default, a pure-lane outcome the
//! `shunt.stage_router.*` series must keep counting as one. That split is the
//! whole reason the map is read against the entry's shape rather than the
//! string alone.
//!
//! # Gated entries
//!
//! `mode = "escalation"` and `type = "advisor"` are the two algorithms that
//! need a *completed* answer before they can decide, so their first
//! `CallModel` is not a judge call at all: it is the turn the caller will be
//! served, made in the caller's own mode and retained ([`GatedKind`],
//! [`drive_gated`]). Their evidence is read by a separate, closed map in
//! [`gated`] — the strings overlap with the table above (`fail_open`) but the
//! outcomes do not, because on a gated entry the question is also *whether a
//! retained turn is served*, not only which target.

pub(crate) mod budget;
mod build;
mod drive;
pub(crate) mod gated;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;

use switchyard_libsy::{Algorithm, RuntimeModels};

use crate::config::{CallBounds, Config, ConfigError};

pub(crate) use build::{check_buildable, check_subagents_buildable};
pub(crate) use drive::drive;
pub(crate) use gated::{drive_gated, GatedDecision};

use budget::JudgeBudget;

/// One built driven entry: the algorithm, everything a drive needs that the
/// algorithm does not hand back, and the per-`(session, agent)` call budget.
pub(crate) struct DrivenEntry {
    /// Long-lived on purpose — see the module docs. Cloned per drive, which is
    /// an `Arc` bump, never a rebuild.
    algorithm: Arc<dyn Algorithm>,
    /// The category → id groups this algorithm routes over. `Arc` because
    /// `drive` takes ownership of one per run.
    models: Arc<RuntimeModels>,
    /// Every id a verdict may name. A selection outside this list is treated
    /// as no verdict at all: routing to an unconfigured id would resolve
    /// through `server.default_provider` as a literal upstream model name.
    targets: Vec<String>,
    /// The id served when no verdict decided the turn.
    fail_open: String,
    /// The six per-call bounds from the entry's own table.
    bounds: CallBounds,
    /// The `algorithm` label on `shunt.router.judge_calls` — the `type` an
    /// operator wrote, or `"subagents"` for an overlay.
    algorithm_label: &'static str,
    budget: JudgeBudget,
    /// `(capable_target, efficient_target)` for a composite entry, and `None`
    /// for every other form. Both the tier a selected id maps to and the
    /// evidence reading above key off this.
    tiers: Option<(String, String)>,
    /// Which retained-turn algorithm this entry runs, or `None` for every
    /// entry that decides before any answer is made — which is every PR 5
    /// form, and the reason those take [`drive`] unchanged.
    gated: Option<GatedKind>,
}

/// The two algorithms whose first model call is the caller's own turn,
/// retained and served after the verdict (ADR-0005 §4).
///
/// Named here rather than inferred from evidence at request time: the request
/// path has to choose the drive *before* libsy has said anything, and the
/// choice decides whether a turn is buffered at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatedKind {
    /// `[models.router] type = "llm_classifier"`, `mode = "escalation"`.
    Escalation,
    /// `[models.router] type = "advisor"`.
    Advisor,
}

impl DrivenEntry {
    /// The metric label for this entry's judge calls.
    pub(crate) fn algorithm_label(&self) -> &'static str {
        self.algorithm_label
    }

    /// Whether a turn on this entry is gated — driven by [`drive_gated`]
    /// rather than [`drive`].
    pub(crate) fn is_gated(&self) -> bool {
        self.gated.is_some()
    }
}

/// Every driven algorithm this config snapshot built, keyed by the `[[models]]`
/// id that carries it.
///
/// Two maps rather than one: a single entry can carry *both* a driven
/// `[models.router]` and a classifier-form `[models.subagents]`, and they are
/// different algorithms deciding different turns — the overlay answers
/// delegated work before the router is reached at all
/// ([`crate::routing::resolve_chain`]).
#[derive(Default)]
pub struct DrivenRouters {
    routers: HashMap<String, DrivenEntry>,
    overlays: HashMap<String, DrivenEntry>,
}

impl DrivenRouters {
    /// Build every driven entry in `config`.
    ///
    /// Runs after `Config::validate`, which has already constructed and
    /// dropped each of these once ([`check_buildable`]), so a failure here is
    /// defensive rather than reachable — and is still an ordinary
    /// [`ConfigError`] so a reload keeps the last good config running instead
    /// of swapping in an entry that cannot answer.
    pub fn build(config: &Config) -> Result<Self, ConfigError> {
        let mut routers = HashMap::new();
        let mut overlays = HashMap::new();
        for model in &config.models {
            if let Some(router) = model.router.as_ref() {
                if let Some(entry) = build::router_entry(router).map_err(|message| {
                    ConfigError::DrivenRouterBuild {
                        model: model.id.clone(),
                        message,
                    }
                })? {
                    routers.insert(model.id.clone(), entry);
                }
            }
            if let Some(classifier) = model.subagents.as_ref().and_then(|o| o.classifier()) {
                let entry = build::overlay_entry(classifier).map_err(|message| {
                    ConfigError::DrivenRouterBuild {
                        model: model.id.clone(),
                        message,
                    }
                })?;
                overlays.insert(model.id.clone(), entry);
            }
        }
        Ok(Self { routers, overlays })
    }

    /// The entry for a driven `[models.router]`, by advertised id. `id` is
    /// expected already stripped of a `[1m]` hint — the same normalization
    /// `resolve_chain` matched the router on.
    pub(crate) fn router(&self, id: &str) -> Option<&DrivenEntry> {
        self.routers.get(id)
    }

    /// The entry for a classifier-form `[models.subagents]`, by advertised id.
    pub(crate) fn overlay(&self, id: &str) -> Option<&DrivenEntry> {
        self.overlays.get(id)
    }
}
